//! 管理代理池的账号稳定绑定；没有健康出口时失败关闭，不回退直连。
use super::super::refresh::refresh_token;
use super::super::types::MultiTokenManager;
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::token_manager::CallContext;
use crate::model::proxy_pool::{ProxyPool, ProxyRoute, ProxyUnavailableError};
use futures::{StreamExt, stream::FuturesUnordered};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

pub(super) struct ProxyReservation<'a> {
    manager: &'a MultiTokenManager,
    id: Option<u64>,
}
impl Drop for ProxyReservation<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            let mut pending = self.manager.proxy_pending_bindings.lock();
            if let Some(count) = pending.get_mut(&id) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    pending.remove(&id);
                }
            }
        }
    }
}

impl MultiTokenManager {
    pub fn attach_proxy_pool(&self, pool: Arc<ProxyPool>) {
        *self.proxy_pool.write() = Some(pool);
    }

    pub fn proxy_bindings(&self) -> HashMap<u64, Vec<u64>> {
        let mut bindings = HashMap::<u64, Vec<u64>>::new();
        for entry in self.entries.lock().iter() {
            if let Some(proxy_id) = entry.credentials.proxy_pool_id {
                bindings.entry(proxy_id).or_default().push(entry.id);
            }
        }
        bindings
    }

    fn binding_counts(&self) -> HashMap<u64, usize> {
        let mut counts: HashMap<u64, usize> = self
            .proxy_bindings()
            .into_iter()
            .map(|(id, accounts)| (id, accounts.len()))
            .collect();
        for (id, count) in self.proxy_pending_bindings.lock().iter() {
            *counts.entry(*id).or_default() += count;
        }
        counts
    }

    /// 导入验证期间也占用分配计数，避免并发的历史账号迁移抢同一个空出口。
    pub(super) fn reserve_new_proxy_binding(
        &self,
        credentials: &mut KiroCredentials,
    ) -> anyhow::Result<ProxyReservation<'_>> {
        let Some(pool) = self.pool() else {
            return Ok(ProxyReservation {
                manager: self,
                id: None,
            });
        };
        if credentials.proxy_pool_id.is_none() {
            return Ok(ProxyReservation {
                manager: self,
                id: None,
            });
        }
        let entries = self.entries.lock();
        if credentials.id.is_some_and(|id| {
            entries.iter().any(|entry| {
                entry.id == id && entry.credentials.proxy_pool_id == credentials.proxy_pool_id
            })
        }) {
            // 已绑定账号的认证材料更新不应触发重平衡。
            return Ok(ProxyReservation {
                manager: self,
                id: None,
            });
        }
        let mut pending = self.proxy_pending_bindings.lock();
        let mut counts = HashMap::<u64, usize>::new();
        for entry in entries.iter() {
            if let Some(id) = entry.credentials.proxy_pool_id {
                *counts.entry(id).or_default() += 1;
            }
        }
        for (id, count) in pending.iter() {
            *counts.entry(*id).or_default() += count;
        }
        let route = pool
            .select_healthy_proxy(&counts, &[])
            .ok_or(ProxyUnavailableError)?;
        apply_route(credentials, &route);
        *pending.entry(route.id).or_default() += 1;
        Ok(ProxyReservation {
            manager: self,
            id: Some(route.id),
        })
    }

    fn pool(&self) -> Option<Arc<ProxyPool>> {
        self.proxy_pool.read().clone()
    }

    /// 给新导入的自动账号分配出口，必须先于 Token 验证和刷新。
    pub(crate) async fn prepare_proxy_binding(
        &self,
        credentials: &mut KiroCredentials,
    ) -> anyhow::Result<()> {
        normalize_manual_proxy(credentials)?;
        if credentials.proxy_pool_id.is_none() && credentials.proxy_url.is_some() {
            return Ok(());
        }
        let Some(pool) = self.pool() else {
            return if credentials.proxy_pool_id.is_some() {
                Err(ProxyUnavailableError.into())
            } else {
                Ok(())
            };
        };
        if !pool.is_configured() && credentials.proxy_pool_id.is_none() {
            return Ok(());
        }
        let counts = self.binding_counts();
        if let Some(id) = credentials.proxy_pool_id {
            if let Some(route) = pool.get_healthy_route(id) {
                apply_route(credentials, &route);
                return Ok(());
            }
            if !pool.route_requires_reassignment(id) {
                let _ = pool.check(id).await;
                if let Some(route) = pool.get_healthy_route(id) {
                    apply_route(credentials, &route);
                    return Ok(());
                }
                if !pool.route_requires_reassignment(id) {
                    return Err(ProxyUnavailableError.into());
                }
            }
        }
        // 新池检查尚未完成时只做有界健康检查，不能先使用未验证出口。
        if pool.select_healthy_proxy(&counts, &[]).is_none() {
            let candidates: Vec<u64> = pool
                .summaries()
                .into_iter()
                .filter(|summary| {
                    summary.enabled
                        && summary
                            .cooldown_until
                            .is_none_or(|due| due <= chrono::Utc::now())
                })
                .map(|summary| summary.id)
                .take(4)
                .collect();
            let checks: FuturesUnordered<_> =
                candidates.into_iter().map(|id| pool.check(id)).collect();
            let _ = tokio::time::timeout(Duration::from_secs(30), async {
                let mut checks = checks;
                while checks.next().await.is_some() {
                    if pool.select_healthy_proxy(&counts, &[]).is_some() {
                        break;
                    }
                }
            })
            .await;
        }
        let excluded: Vec<u64> = credentials.proxy_pool_id.into_iter().collect();
        let route = pool
            .select_healthy_proxy(&counts, &excluded)
            .ok_or(ProxyUnavailableError)?;
        apply_route(credentials, &route);
        Ok(())
    }

    pub(crate) async fn ensure_proxy_route(&self, id: u64) -> anyhow::Result<KiroCredentials> {
        let (mut credentials, generation) = {
            let entries = self.entries.lock();
            let entry = entries
                .iter()
                .find(|entry| entry.id == id)
                .ok_or(ProxyUnavailableError)?;
            (entry.credentials.clone(), entry.recovery_generation)
        };
        let before = credentials.clone();
        self.prepare_proxy_binding(&mut credentials).await?;
        let changed = credentials.proxy_pool_id != before.proxy_pool_id
            || credentials.proxy_pool_generation != before.proxy_pool_generation
            || credentials.proxy_url != before.proxy_url
            || credentials.proxy_username != before.proxy_username
            || credentials.proxy_password != before.proxy_password;
        if !changed {
            return Ok(credentials);
        }
        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|entry| entry.id == id)
                .ok_or(ProxyUnavailableError)?;
            if entry.recovery_generation != generation
                || !Self::same_outbound_identity(&entry.credentials, &before)
            {
                return Err(ProxyUnavailableError.into());
            }
            // 健康检查不持有账号锁；真正提交分配时重新计算，保证优先独占。
            let counts: HashMap<u64, usize> = entries
                .iter()
                .filter_map(|entry| entry.credentials.proxy_pool_id)
                .fold(
                    self.proxy_pending_bindings.lock().clone(),
                    |mut counts, id| {
                        *counts.entry(id).or_default() += 1;
                        counts
                    },
                );
            if before.proxy_pool_id != credentials.proxy_pool_id
                && let Some(pool) = self.pool()
            {
                let exclude: Vec<_> = before.proxy_pool_id.into_iter().collect();
                let route = pool
                    .select_healthy_proxy(&counts, &exclude)
                    .ok_or(ProxyUnavailableError)?;
                apply_route(&mut credentials, &route);
            }
            let entry = entries
                .iter_mut()
                .find(|entry| entry.id == id)
                .ok_or(ProxyUnavailableError)?;
            apply_proxy_fields(&mut entry.credentials, &credentials);
            // 只使旧请求失效，不改变认证禁用、失败计数或恢复截止时间。
            entry.recovery_generation = entry.recovery_generation.wrapping_add(1);
        }
        self.persist_credentials()?;
        Ok(credentials)
    }

    pub(crate) fn managed_route_is_healthy(&self, credentials: &KiroCredentials) -> bool {
        let Some(pool) = self.pool() else {
            return credentials.proxy_pool_id.is_none();
        };
        match credentials.proxy_pool_id {
            Some(id) => pool.get_healthy_route(id).is_some_and(|route| {
                credentials.proxy_pool_generation == Some(route.generation)
                    && credentials.proxy_url.as_deref() == Some(route.url.as_str())
                    && credentials.proxy_username == route.username
                    && credentials.proxy_password == route.password
            }),
            None => credentials.proxy_url.is_some() || !pool.is_configured(),
        }
    }

    /// 仅在出口故障被独立健康探测确认后更换绑定，不把上游业务错误当代理故障。
    pub(crate) async fn confirm_proxy_failure(
        &self,
        context: &CallContext,
    ) -> anyhow::Result<bool> {
        let Some(proxy_id) = context.credentials.proxy_pool_id else {
            return Ok(false);
        };
        let Some(pool) = self.pool() else {
            return Err(ProxyUnavailableError.into());
        };
        // 在锁内只检查原配置，不能用健康状态检查拒绝刚刚故障的旧绑定。
        let generation = {
            let entries = self.entries.lock();
            let Some(entry) = entries.iter().find(|entry| entry.id == context.id) else {
                return Ok(false);
            };
            if !Self::same_outbound_identity(&entry.credentials, &context.credentials) {
                return Ok(false);
            }
            entry.recovery_generation
        };
        if pool
            .confirm_failure(proxy_id)
            .await
            .map_err(|_| ProxyUnavailableError)?
        {
            return Ok(false);
        }
        {
            let mut entries = self.entries.lock();
            let Some(entry) = entries.iter_mut().find(|entry| entry.id == context.id) else {
                return Ok(false);
            };
            if entry.recovery_generation != generation
                || !Self::same_outbound_identity(&entry.credentials, &context.credentials)
            {
                return Ok(false);
            }
            let counts: HashMap<u64, usize> = entries
                .iter()
                .filter_map(|entry| entry.credentials.proxy_pool_id)
                .fold(
                    self.proxy_pending_bindings.lock().clone(),
                    |mut counts, id| {
                        *counts.entry(id).or_default() += 1;
                        counts
                    },
                );
            let route = pool
                .select_healthy_proxy(&counts, &[proxy_id])
                .ok_or(ProxyUnavailableError)?;
            let entry = entries
                .iter_mut()
                .find(|entry| entry.id == context.id)
                .ok_or(ProxyUnavailableError)?;
            apply_route(&mut entry.credentials, &route);
            entry.recovery_generation = entry.recovery_generation.wrapping_add(1);
        }
        self.persist_credentials()?;
        Ok(true)
    }

    pub(crate) fn schedule_proxy_body_failure(self: &Arc<Self>, context: &CallContext) {
        if context.credentials.proxy_pool_id.is_none() {
            return;
        }
        let manager = self.clone();
        let context = context.clone();
        tokio::spawn(async move {
            let _ = manager.confirm_proxy_failure(&context).await;
        });
    }

    pub(crate) async fn refresh_with_managed_route(
        &self,
        id: u64,
        credentials: &KiroCredentials,
    ) -> anyhow::Result<KiroCredentials> {
        let proxy = credentials.effective_proxy(self.proxy.as_ref());
        match refresh_token(credentials, &self.config, proxy.as_ref()).await {
            Ok(refreshed) => Ok(refreshed),
            Err(error)
                if credentials.proxy_pool_id.is_some() && proxy_transport_failure(&error) =>
            {
                let context = CallContext {
                    id,
                    credentials: credentials.clone(),
                    token: credentials.access_token.clone().unwrap_or_default(),
                };
                if self.confirm_proxy_failure(&context).await? {
                    let current = self.ensure_proxy_route(id).await?;
                    let proxy = current.effective_proxy(self.proxy.as_ref());
                    let result = refresh_token(&current, &self.config, proxy.as_ref()).await;
                    if let Err(error) = &result
                        && proxy_transport_failure(error)
                    {
                        let current_context = CallContext {
                            id,
                            token: current.access_token.clone().unwrap_or_default(),
                            credentials: current,
                        };
                        if self.confirm_proxy_failure(&current_context).await? {
                            return Err(ProxyUnavailableError.into());
                        }
                    }
                    result
                } else {
                    Err(error)
                }
            }
            Err(error) => Err(error),
        }
    }

    pub(super) async fn validate_candidate_with_proxy<'a>(
        &'a self,
        credentials: &mut KiroCredentials,
        reservation: &mut ProxyReservation<'a>,
    ) -> anyhow::Result<KiroCredentials> {
        for attempt in 0..2 {
            let proxy = credentials.effective_proxy(self.proxy.as_ref());
            match refresh_token(credentials, &self.config, proxy.as_ref()).await {
                Ok(validated) => return Ok(validated),
                Err(error)
                    if attempt == 0
                        && credentials.proxy_pool_id.is_some()
                        && proxy_transport_failure(&error) =>
                {
                    let pool = self.pool().ok_or(ProxyUnavailableError)?;
                    if pool
                        .confirm_failure(credentials.proxy_pool_id.unwrap())
                        .await
                        .map_err(|_| ProxyUnavailableError)?
                    {
                        return Err(error);
                    }
                    self.prepare_proxy_binding(credentials).await?;
                    *reservation = self.reserve_new_proxy_binding(credentials)?;
                }
                Err(error)
                    if credentials.proxy_pool_id.is_some() && proxy_transport_failure(&error) =>
                {
                    let pool = self.pool().ok_or(ProxyUnavailableError)?;
                    if !pool
                        .confirm_failure(credentials.proxy_pool_id.unwrap())
                        .await
                        .map_err(|_| ProxyUnavailableError)?
                    {
                        return Err(ProxyUnavailableError.into());
                    }
                    return Err(error);
                }
                Err(error) => return Err(error),
            }
        }
        Err(ProxyUnavailableError.into())
    }

    pub(crate) fn proxy_snapshot(
        &self,
        credentials: &KiroCredentials,
    ) -> (Option<String>, String, Option<String>, Option<String>) {
        if credentials.proxy_pool_id.is_some() {
            let status = if self
                .pool()
                .and_then(|pool| pool.summary(credentials.proxy_pool_id.unwrap()))
                .is_some_and(|summary| {
                    summary.checking
                        || summary.status == crate::model::proxy_pool::ProxyStatus::Unknown
                }) {
                "pending"
            } else if self.managed_route_is_healthy(credentials) {
                "healthy"
            } else {
                "unavailable"
            };
            return (
                Some(status.to_string()),
                "pool".to_string(),
                credentials
                    .proxy_url
                    .as_deref()
                    .map(crate::http_client::safe_proxy_address),
                None,
            );
        }
        let mode = match credentials.proxy_url.as_deref() {
            Some(value) if value.eq_ignore_ascii_case(KiroCredentials::PROXY_DIRECT) => "direct",
            Some(_) => "manual",
            None if self.pool().is_some_and(|pool| pool.is_configured()) => "pool",
            None => "global",
        };
        if mode == "pool" {
            return (Some("pending".to_string()), mode.to_string(), None, None);
        }
        let effective = credentials
            .effective_proxy(self.proxy.as_ref())
            .map(|proxy| proxy.url);
        (None, mode.to_string(), effective, None)
    }
}

/// 把旧内联认证移到私有认证字段，公开地址和重新编辑不会泄漏/丢失密码。
pub(crate) fn normalize_manual_proxy(credentials: &mut KiroCredentials) -> anyhow::Result<bool> {
    if credentials.proxy_pool_id.is_some() {
        return Ok(false);
    }
    let Some(raw) = credentials.proxy_url.as_deref() else {
        return Ok(false);
    };
    if raw.trim().is_empty() {
        credentials.proxy_url = None;
        return Ok(true);
    }
    if raw.eq_ignore_ascii_case(KiroCredentials::PROXY_DIRECT) {
        return Ok(false);
    }
    let mut parsed =
        reqwest::Url::parse(raw).map_err(|_| anyhow::anyhow!("Invalid manual proxy address."))?;
    if parsed.username().is_empty() && parsed.password().is_none() {
        return Ok(false);
    }
    if credentials
        .proxy_username
        .as_deref()
        .is_none_or(str::is_empty)
    {
        credentials.proxy_username = Some(
            urlencoding::decode(parsed.username())
                .map_err(|_| anyhow::anyhow!("Invalid manual proxy authentication."))?
                .into_owned(),
        );
    }
    if credentials
        .proxy_password
        .as_deref()
        .is_none_or(str::is_empty)
        && let Some(password) = parsed.password()
    {
        credentials.proxy_password = Some(
            urlencoding::decode(password)
                .map_err(|_| anyhow::anyhow!("Invalid manual proxy authentication."))?
                .into_owned(),
        );
    }
    let _ = parsed.set_username("");
    let _ = parsed.set_password(None);
    credentials.proxy_url = Some(parsed.to_string());
    Ok(true)
}

pub(crate) fn proxy_transport_failure(error: &anyhow::Error) -> bool {
    error.chain().any(|source| {
        source
            .downcast_ref::<reqwest::Error>()
            .is_some_and(|error| {
                error.is_connect()
                    || error.is_timeout()
                    || error.status().is_some_and(|status| status.as_u16() == 407)
            })
    })
}

fn apply_route(credentials: &mut KiroCredentials, route: &ProxyRoute) {
    credentials.proxy_pool_id = Some(route.id);
    credentials.proxy_pool_generation = Some(route.generation);
    credentials.proxy_url = Some(route.url.clone());
    credentials.proxy_username = route.username.clone();
    credentials.proxy_password = route.password.clone();
}

fn apply_proxy_fields(target: &mut KiroCredentials, source: &KiroCredentials) {
    target.proxy_pool_id = source.proxy_pool_id;
    target.proxy_pool_generation = source.proxy_pool_generation;
    target.proxy_url = source.proxy_url.clone();
    target.proxy_username = source.proxy_username.clone();
    target.proxy_password = source.proxy_password.clone();
}
