//! 自动禁用账号的冷却和只读恢复探测；不会发送推理请求。
use super::super::entry::{CredentialEntry, DisabledReason};
use super::super::refresh::{RefreshTokenInvalidError, is_token_expired};
use super::super::types::MultiTokenManager;
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::model::usage_limits::UsageLimitsResponse;
use chrono::{DateTime, Duration, Utc};
use std::future::Future;
use std::time::Duration as StdDuration;

/// 每轮最多四个顺序探测，避免大账号池一次触发大量控制面请求。
const MAX_RECOVERY_PROBES_PER_CYCLE: usize = 4;

struct RecoveryTicket {
    id: u64,
    generation: u64,
    reason: DisabledReason,
    credentials: KiroCredentials,
}

/// 超时、取消和正常返回都释放 singleflight，不能让账号永久卡在探测中。
struct ProbeGuard<'a> {
    manager: &'a MultiTokenManager,
    id: u64,
}

impl Drop for ProbeGuard<'_> {
    fn drop(&mut self) {
        if let Some(entry) = self
            .manager
            .entries
            .lock()
            .iter_mut()
            .find(|e| e.id == self.id)
        {
            entry.recovery_probe_in_flight = false;
        }
    }
}

impl MultiTokenManager {
    fn recovery_delay(&self, failures: u32) -> Duration {
        let base = self.config.account_recovery_cooldown_secs.max(1);
        let maximum = self.config.account_recovery_max_cooldown_secs.max(base);
        let seconds = base
            .saturating_mul(1_u64.checked_shl(failures.min(63)).unwrap_or(u64::MAX))
            .min(maximum)
            .min(604_800);
        Duration::seconds(seconds as i64)
    }

    pub(crate) fn start_recovery_cooldown(&self, entry: &mut CredentialEntry, now: DateTime<Utc>) {
        entry.invalidate_recovery();
        if entry
            .disabled_reason
            .is_some_and(DisabledReason::allows_recovery_probe)
        {
            entry.recovery_next_probe_at = now.checked_add_signed(self.recovery_delay(0));
        }
    }

    pub(crate) fn initialize_recovery_cooldowns(&self) {
        self.initialize_recovery_cooldowns_at(Utc::now());
    }

    fn initialize_recovery_cooldowns_at(&self, now: DateTime<Utc>) {
        let changed = {
            let mut entries = self.entries.lock();
            let mut changed = false;
            for entry in entries.iter_mut() {
                if entry.disabled
                    && entry
                        .disabled_reason
                        .is_some_and(DisabledReason::allows_recovery_probe)
                    && entry.recovery_next_probe_at.is_none()
                {
                    // 旧数据缺时间戳也要先冷却，并立即落盘；重启不得反复从零开始。
                    entry.recovery_next_probe_at =
                        now.checked_add_signed(self.recovery_delay(entry.recovery_failures));
                    changed = true;
                }
            }
            changed
        };
        if changed {
            self.save_stats();
        }
    }

    fn claim_recovery_probe(&self, id: u64, now: DateTime<Utc>) -> Option<RecoveryTicket> {
        let mut entries = self.entries.lock();
        let entry = entries.iter_mut().find(|e| e.id == id)?;
        let reason = entry.disabled_reason?;
        if !entry.disabled
            || !reason.allows_recovery_probe()
            || entry.recovery_probe_in_flight
            || entry.recovery_next_probe_at.is_none_or(|due| due > now)
        {
            return None;
        }
        entry.recovery_probe_in_flight = true;
        // 在探测前保存新的冷却截止，取消/进程退出也不会立刻重新探测。
        entry.recovery_next_probe_at =
            now.checked_add_signed(self.recovery_delay(entry.recovery_failures));
        Some(RecoveryTicket {
            id,
            generation: entry.recovery_generation,
            reason,
            credentials: entry.credentials.clone(),
        })
    }

    /// 由后台任务调用；仅探测到期的自动禁用账号，手动/永久失效账号从不探测。
    pub async fn recover_due_accounts(&self) -> usize {
        self.recover_due_accounts_with(|id| self.get_usage_limits_for(id), Utc::now)
            .await
    }

    pub(super) async fn recover_due_accounts_with<P, F, C>(&self, probe: P, clock: C) -> usize
    where
        P: Fn(u64) -> F,
        F: Future<Output = anyhow::Result<UsageLimitsResponse>>,
        C: Fn() -> DateTime<Utc>,
    {
        self.initialize_recovery_cooldowns_at(clock());
        let due_ids: Vec<u64> = {
            let now = clock();
            self.entries
                .lock()
                .iter()
                .filter(|e| {
                    e.disabled
                        && !e.recovery_probe_in_flight
                        && e.disabled_reason
                            .is_some_and(DisabledReason::allows_recovery_probe)
                        && e.recovery_next_probe_at.is_some_and(|due| due <= now)
                })
                .map(|e| e.id)
                .take(MAX_RECOVERY_PROBES_PER_CYCLE)
                .collect()
        };
        let mut recovered = 0;
        for id in due_ids {
            // 出口更新先于恢复 ticket，合法代理切换不会使本轮只读探测结果失效。
            if self.ensure_proxy_route(id).await.is_err() {
                continue;
            }
            let Some(ticket) = self.claim_recovery_probe(id, clock()) else {
                continue;
            };
            let _guard = ProbeGuard { manager: self, id };
            self.save_stats();
            // 不持有 entries/admin/global 恢复互斥锁等待网络。
            let result = tokio::time::timeout(
                StdDuration::from_secs(self.config.account_recovery_probe_timeout_secs.max(1)),
                probe(id),
            )
            .await;
            let finished_at = clock();
            let permanent_invalid = result.as_ref().is_ok_and(|r| {
                r.as_ref()
                    .is_err_and(|e| e.downcast_ref::<RefreshTokenInvalidError>().is_some())
            });
            let healthy = result.as_ref().is_ok_and(|r| {
                r.as_ref()
                    .is_ok_and(|usage| usage_has_available_quota(usage, finished_at))
            });
            {
                let mut entries = self.entries.lock();
                let Some(entry) = entries.iter_mut().find(|e| e.id == ticket.id) else {
                    continue;
                };
                if !entry.disabled
                    || entry.recovery_generation != ticket.generation
                    || entry.disabled_reason != Some(ticket.reason)
                    || !same_recovery_identity(&ticket.credentials, &entry.credentials)
                {
                    // 管理员编辑/启停/删除后的旧结果不能重新启用账号。
                    continue;
                }
                if permanent_invalid {
                    entry.disabled_reason = Some(DisabledReason::InvalidRefreshToken);
                    entry.invalidate_recovery();
                } else if healthy
                    && !is_token_expired(&entry.credentials)
                    && entry
                        .credentials
                        .access_token
                        .as_deref()
                        .is_some_and(|token| !token.trim().is_empty())
                {
                    entry.disabled = false;
                    entry.disabled_reason = None;
                    entry.failure_count = 0;
                    entry.refresh_failure_count = 0;
                    entry.quota_exhausted_at = None;
                    entry.invalidate_recovery();
                    recovered += 1;
                } else {
                    entry.recovery_failures = entry.recovery_failures.saturating_add(1);
                    entry.recovery_next_probe_at = finished_at
                        .checked_add_signed(self.recovery_delay(entry.recovery_failures));
                }
            }
            self.save_stats();
        }
        if recovered > 0 {
            self.select_highest_priority();
            tracing::info!(recovered, "只读账号和额度探测通过，恢复自动禁用账号");
        }
        recovered
    }
}

/// access/refresh Token、有效期、profileArn 和订阅信息允许在只读探测中合法刷新。
/// 管理员替换这些字段也会递增 generation；额外比较固定身份和出站配置防止旧结果串号。
fn same_recovery_identity(before: &KiroCredentials, after: &KiroCredentials) -> bool {
    before.machine_id == after.machine_id
        && before.auth_method == after.auth_method
        && before.client_id == after.client_id
        && before.client_secret == after.client_secret
        && before.token_endpoint == after.token_endpoint
        && before.scopes == after.scopes
        && before.region == after.region
        && before.auth_region == after.auth_region
        && before.api_region == after.api_region
        && before.proxy_url == after.proxy_url
        && before.proxy_username == after.proxy_username
        && before.proxy_password == after.proxy_password
}

/// HTTP 200 不足以恢复：必须有已知、有效且剩余大于零的可用额度。
pub(super) fn usage_has_available_quota(usage: &UsageLimitsResponse, now: DateTime<Utc>) -> bool {
    let Some(primary) = usage.usage_breakdown_list.first() else {
        return false;
    };
    let numeric = |precise: f64, integer: i64| -> Option<f64> {
        if !precise.is_finite() || precise < 0.0 || integer < 0 {
            return None;
        }
        Some(if precise > 0.0 {
            precise
        } else {
            integer as f64
        })
    };
    let Some(mut limit) = numeric(primary.usage_limit_with_precision, primary.usage_limit) else {
        return false;
    };
    let Some(mut used) = numeric(primary.current_usage_with_precision, primary.current_usage)
    else {
        return false;
    };
    if let Some(trial) = &primary.free_trial_info
        && trial.is_active()
        && trial
            .free_trial_expiry
            .is_none_or(|expiry| expiry.is_finite() && expiry > now.timestamp() as f64)
    {
        let Some(trial_limit) = numeric(trial.usage_limit_with_precision, trial.usage_limit) else {
            return false;
        };
        let Some(trial_used) = numeric(trial.current_usage_with_precision, trial.current_usage)
        else {
            return false;
        };
        limit += trial_limit;
        used += trial_used;
    }
    for bonus in primary.bonuses.iter().filter(|bonus| bonus.is_active()) {
        if !bonus.usage_limit.is_finite()
            || !bonus.current_usage.is_finite()
            || bonus.usage_limit < 0.0
            || bonus.current_usage < 0.0
        {
            return false;
        }
        limit += bonus.usage_limit;
        used += bonus.current_usage;
    }
    limit.is_finite() && used.is_finite() && limit > 0.0 && limit > used
}
