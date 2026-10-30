//! 恢复状态机测试：仅注入本地 probe，不调用上游。
use super::recovery::usage_has_available_quota;
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::model::usage_limits::UsageLimitsResponse;
use crate::kiro::token_manager::entry::DisabledReason;
use crate::kiro::token_manager::refresh::RefreshTokenInvalidError;
use crate::kiro::token_manager::types::{MAX_FAILURES_PER_CREDENTIAL, MultiTokenManager};
use crate::model::config::Config;
use chrono::{Duration, Utc};
use std::future::{pending, ready};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Notify;

struct TempDirGuard(std::path::PathBuf);

impl TempDirGuard {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(name);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn make_valid_cred(token: &str) -> KiroCredentials {
    KiroCredentials {
        access_token: Some(token.to_string()),
        refresh_token: Some(format!("test-{token}-{}", "r".repeat(120))),
        expires_at: Some((Utc::now() + Duration::hours(1)).to_rfc3339()),
        ..KiroCredentials::default()
    }
}

fn config() -> Config {
    let mut config = Config::default();
    config.account_recovery_cooldown_secs = 10;
    config.account_recovery_max_cooldown_secs = 40;
    config.account_recovery_probe_timeout_secs = 1;
    config
}

fn manager(count: usize) -> MultiTokenManager {
    MultiTokenManager::new(
        config(),
        (0..count)
            .map(|i| make_valid_cred(&format!("test-{i}")))
            .collect(),
        None,
        None,
        true,
    )
    .unwrap()
}

fn healthy_usage() -> UsageLimitsResponse {
    serde_json::from_value(serde_json::json!({
        "usageBreakdownList": [{"usageLimitWithPrecision": 100.0, "currentUsageWithPrecision": 10.0}]
    })).unwrap()
}

fn fail_account(manager: &MultiTokenManager, id: u64) {
    for _ in 0..MAX_FAILURES_PER_CREDENTIAL {
        manager.report_failure(id);
    }
}

fn due(manager: &MultiTokenManager, id: u64) -> chrono::DateTime<Utc> {
    manager
        .entries
        .lock()
        .iter()
        .find(|e| e.id == id)
        .unwrap()
        .recovery_next_probe_at
        .unwrap()
}

#[tokio::test]
async fn all_unavailable_and_bound_pool_do_not_bypass_cooldown() {
    let manager = manager(2);
    fail_account(&manager, 1);
    assert!(manager.acquire_context_filtered(None, &[1]).await.is_err());
    assert_eq!(manager.acquire_context(None).await.unwrap().id, 2);
    fail_account(&manager, 2);
    assert!(manager.acquire_context(None).await.is_err());
    assert_eq!(manager.available_count(), 0);
    let now = due(&manager, 1) - Duration::seconds(1);
    assert_eq!(
        manager
            .recover_due_accounts_with(|_| async { panic!("cooldown must not probe") }, || now)
            .await,
        0
    );
}

#[tokio::test]
async fn token_acquisition_does_not_clear_sticky_throttles_but_completed_success_does() {
    let manager = manager(1);
    manager
        .acquire_context_sticky(None, &[], Some("sticky-recovery-test"), &[])
        .await
        .unwrap();
    assert!(!manager.report_sticky_throttled("sticky-recovery-test", 1));
    manager
        .acquire_context_sticky(None, &[], Some("sticky-recovery-test"), &[])
        .await
        .unwrap();
    assert_eq!(
        manager.sticky_cache.lock()["sticky-recovery-test"].consecutive_throttles,
        1
    );
    manager.report_success(1);
    assert_eq!(
        manager.sticky_cache.lock()["sticky-recovery-test"].consecutive_throttles,
        0
    );
}

#[tokio::test]
async fn late_body_success_cannot_change_disabled_account_or_cooldown() {
    let manager = manager(1);
    manager
        .acquire_context_sticky(None, &[], Some("disabled-stream"), &[])
        .await
        .unwrap();
    manager.report_sticky_throttled("disabled-stream", 1);
    fail_account(&manager, 1);
    let next = due(&manager, 1);
    assert!(!manager.credential_is_available(1));
    manager.report_success(1);
    {
        let entries = manager.entries.lock();
        assert_eq!(entries[0].failure_count, MAX_FAILURES_PER_CREDENTIAL);
        assert_eq!(entries[0].success_count, 0);
        assert_eq!(entries[0].recovery_next_probe_at, Some(next));
        assert_eq!(
            entries[0].disabled_reason,
            Some(DisabledReason::TooManyFailures)
        );
    }
    assert_eq!(
        manager.sticky_cache.lock()["disabled-stream"].consecutive_throttles,
        1
    );
    manager.set_disabled(1, true).await.unwrap();
    manager.report_success(1);
    assert_eq!(
        manager.entries.lock()[0].disabled_reason,
        Some(DisabledReason::Manual)
    );
    assert_eq!(manager.entries.lock()[0].recovery_next_probe_at, None);
    manager.delete_credential(1).await.unwrap();
    assert!(!manager.credential_is_available(1));
}

#[tokio::test]
async fn stale_context_health_reports_cannot_penalize_replacement_identity() {
    let manager = manager(1);
    let old = manager.acquire_context(None).await.unwrap();
    assert!(manager.context_is_current(&old));
    {
        let mut entries = manager.entries.lock();
        entries[0].credentials.access_token = Some("replacement-access".to_string());
        entries[0].credentials.refresh_token = Some("replacement-refresh".repeat(10));
        entries[0].invalidate_recovery();
    }
    assert!(!manager.context_is_current(&old));
    manager.report_failure(1);
    manager.report_success_for_context(&old);
    manager.report_failure_for_context(&old);
    manager.report_quota_for_context(&old);
    manager.report_profile_missing_for_context(&old);
    manager.report_throttled_for_context(&old);
    manager.report_rotation_for_context(&old);
    manager.report_refresh_failure_for_credentials(old.id, &old.credentials);
    manager.report_invalid_refresh_for_credentials(old.id, &old.credentials);
    {
        let entries = manager.entries.lock();
        assert!(!entries[0].disabled);
        assert_eq!(entries[0].failure_count, 1);
        assert_eq!(entries[0].refresh_failure_count, 0);
        assert_eq!(entries[0].throttle_count, 0);
        assert_eq!(entries[0].rotation_bias, 0);
        assert_eq!(entries[0].success_count, 0);
    }
    let current = manager.acquire_context(None).await.unwrap();
    assert!(manager.context_is_current(&current));
    manager.report_success_for_context(&current);
    assert_eq!(manager.entries.lock()[0].failure_count, 0);
    assert_eq!(manager.entries.lock()[0].success_count, 1);
}

#[test]
fn invalid_refresh_upgrades_automatic_disable_and_survives_restart() {
    let dir = TempDirGuard::new(&format!("recovery-permanent-{}", uuid::Uuid::new_v4()));
    let path = dir.path().join("credentials.json");
    let first = MultiTokenManager::new(
        config(),
        vec![make_valid_cred("restart")],
        None,
        Some(path.clone()),
        true,
    )
    .unwrap();
    let credentials = first.entries.lock()[0].credentials.clone();
    fail_account(&first, 1);
    assert!(first.entries.lock()[0].recovery_next_probe_at.is_some());
    first.report_invalid_refresh_for_credentials(1, &credentials);
    {
        let entries = first.entries.lock();
        assert_eq!(
            entries[0].disabled_reason,
            Some(DisabledReason::InvalidRefreshToken)
        );
        assert_eq!(entries[0].recovery_next_probe_at, None);
    }
    let second =
        MultiTokenManager::new(config(), vec![credentials], None, Some(path), true).unwrap();
    assert_eq!(
        second.entries.lock()[0].disabled_reason,
        Some(DisabledReason::InvalidRefreshToken)
    );
    assert_eq!(second.entries.lock()[0].recovery_next_probe_at, None);
}

#[tokio::test]
async fn invalid_refresh_never_overrides_manual_disable() {
    let manager = manager(1);
    let credentials = manager.entries.lock()[0].credentials.clone();
    manager.set_disabled(1, true).await.unwrap();
    manager.report_invalid_refresh_for_credentials(1, &credentials);
    assert_eq!(
        manager.entries.lock()[0].disabled_reason,
        Some(DisabledReason::Manual)
    );
}

#[tokio::test]
async fn failed_probe_backoff_is_bounded_and_does_not_enable() {
    let manager = manager(1);
    fail_account(&manager, 1);
    for attempt in 1..=5 {
        let now = due(&manager, 1);
        assert_eq!(
            manager
                .recover_due_accounts_with(
                    |_| ready(Err(anyhow::anyhow!("offline probe failure"))),
                    || now
                )
                .await,
            0
        );
        let entry = &manager.entries.lock()[0];
        assert!(entry.disabled);
        assert_eq!(entry.recovery_failures, attempt);
        let expected = (10_i64 * (1_i64 << attempt)).min(40);
        assert_eq!(
            entry.recovery_next_probe_at.unwrap(),
            now + Duration::seconds(expected)
        );
        assert!(!entry.recovery_probe_in_flight);
    }
}

#[tokio::test]
async fn success_requires_real_positive_quota_and_valid_token() {
    let manager = manager(1);
    fail_account(&manager, 1);
    let now = due(&manager, 1);
    let empty: UsageLimitsResponse = serde_json::from_str("{}").unwrap();
    assert_eq!(
        manager
            .recover_due_accounts_with(|_| ready(Ok(empty.clone())), || now)
            .await,
        0
    );
    let now = due(&manager, 1);
    manager.entries.lock()[0].credentials.expires_at =
        Some((Utc::now() - Duration::hours(1)).to_rfc3339());
    assert_eq!(
        manager
            .recover_due_accounts_with(|_| ready(Ok(healthy_usage())), || now)
            .await,
        0
    );
    manager.entries.lock()[0].credentials.expires_at =
        Some((Utc::now() + Duration::hours(1)).to_rfc3339());
    let now = due(&manager, 1);
    assert_eq!(
        manager
            .recover_due_accounts_with(|_| ready(Ok(healthy_usage())), || now)
            .await,
        1
    );
    let entry = &manager.entries.lock()[0];
    assert!(!entry.disabled);
    assert_eq!(entry.disabled_reason, None);
    assert_eq!(entry.failure_count, 0);
    assert_eq!(entry.refresh_failure_count, 0);
    assert_eq!(entry.recovery_next_probe_at, None);
    assert_eq!(entry.recovery_failures, 0);
}

#[test]
fn quota_validation_rejects_unknown_exhausted_nonfinite_and_expired_trial() {
    let now = Utc::now();
    for body in [
        serde_json::json!({}),
        serde_json::json!({"usageBreakdownList":[{"usageLimit":10,"currentUsage":10}]}),
        serde_json::json!({"usageBreakdownList":[{"usageLimit":-1}]}),
        serde_json::json!({"usageBreakdownList":[{"freeTrialInfo":{"freeTrialStatus":"ACTIVE","freeTrialExpiry":now.timestamp()-1,"usageLimit":10}}]}),
    ] {
        let usage: UsageLimitsResponse = serde_json::from_value(body).unwrap();
        assert!(!usage_has_available_quota(&usage, now));
    }
    let integer_only: UsageLimitsResponse = serde_json::from_value(
        serde_json::json!({"usageBreakdownList":[{"usageLimit":10,"currentUsage":1}]}),
    )
    .unwrap();
    assert!(usage_has_available_quota(&integer_only, now));
    let bonus: UsageLimitsResponse = serde_json::from_value(serde_json::json!({"usageBreakdownList":[{"bonuses":[{"status":"ACTIVE","usageLimit":10,"currentUsage":1}]}]})).unwrap();
    assert!(usage_has_available_quota(&bonus, now));
    let mut invalid = healthy_usage();
    invalid.usage_breakdown_list[0].usage_limit_with_precision = f64::NAN;
    assert!(!usage_has_available_quota(&invalid, now));
}

#[tokio::test]
async fn permanent_and_manual_states_never_probe() {
    let manager = manager(3);
    manager.set_disabled(1, true).await.unwrap();
    manager.report_refresh_token_invalid(2);
    manager.report_profile_arn_missing(3);
    let future = Utc::now() + Duration::days(400);
    assert_eq!(
        manager
            .recover_due_accounts_with(
                |_| async { panic!("permanent/manual must not probe") },
                || future
            )
            .await,
        0
    );
    assert_eq!(manager.available_count(), 0);
    assert!(
        manager
            .entries
            .lock()
            .iter()
            .all(|e| e.recovery_next_probe_at.is_none())
    );
}

#[tokio::test]
async fn invalid_refresh_during_probe_stays_permanently_disabled() {
    let manager = manager(1);
    fail_account(&manager, 1);
    let now = due(&manager, 1);
    assert_eq!(
        manager
            .recover_due_accounts_with(
                |_| ready(Err(anyhow::Error::new(RefreshTokenInvalidError))),
                || now
            )
            .await,
        0
    );
    let entry = &manager.entries.lock()[0];
    assert!(entry.disabled);
    assert_eq!(
        entry.disabled_reason,
        Some(DisabledReason::InvalidRefreshToken)
    );
    assert_eq!(entry.recovery_next_probe_at, None);
}

#[tokio::test]
async fn concurrent_cycles_use_one_probe_per_account() {
    let manager = Arc::new(manager(1));
    fail_account(&manager, 1);
    let now = due(&manager, 1);
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let task = tokio::spawn({
        let manager = manager.clone();
        let entered = entered.clone();
        let release = release.clone();
        let calls = calls.clone();
        async move {
            manager
                .recover_due_accounts_with(
                    |_| {
                        let entered = entered.clone();
                        let release = release.clone();
                        let calls = calls.clone();
                        async move {
                            calls.fetch_add(1, Ordering::Relaxed);
                            entered.notify_one();
                            release.notified().await;
                            Ok(healthy_usage())
                        }
                    },
                    || now,
                )
                .await
        }
    });
    entered.notified().await;
    assert!(manager.entries.lock()[0].recovery_probe_in_flight);
    assert_eq!(
        manager
            .recover_due_accounts_with(
                |_| async { panic!("duplicate probe") },
                || now + Duration::seconds(100)
            )
            .await,
        0
    );
    release.notify_one();
    assert_eq!(task.await.unwrap(), 1);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn manual_override_and_delete_during_probe_cannot_resurrect() {
    for delete in [false, true] {
        let manager = Arc::new(manager(1));
        fail_account(&manager, 1);
        let now = due(&manager, 1);
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let task = tokio::spawn({
            let manager = manager.clone();
            let entered = entered.clone();
            let release = release.clone();
            async move {
                manager
                    .recover_due_accounts_with(
                        |_| {
                            let entered = entered.clone();
                            let release = release.clone();
                            async move {
                                entered.notify_one();
                                release.notified().await;
                                Ok(healthy_usage())
                            }
                        },
                        || now,
                    )
                    .await
            }
        });
        entered.notified().await;
        if delete {
            manager.delete_credential(1).await.unwrap();
        } else {
            manager.set_disabled(1, true).await.unwrap();
        }
        release.notify_one();
        assert_eq!(task.await.unwrap(), 0);
        assert_eq!(manager.available_count(), 0);
        if !delete {
            assert_eq!(
                manager.entries.lock()[0].disabled_reason,
                Some(DisabledReason::Manual)
            );
            assert!(!manager.entries.lock()[0].recovery_probe_in_flight);
        }
    }
}

#[tokio::test]
async fn identity_snapshot_and_generation_reject_stale_probe_results() {
    for generation_change in [false, true] {
        let manager = manager(1);
        fail_account(&manager, 1);
        let now = due(&manager, 1);
        assert_eq!(
            manager
                .recover_due_accounts_with(
                    |_| async {
                        let mut entries = manager.entries.lock();
                        if generation_change {
                            entries[0].invalidate_recovery();
                        } else {
                            entries[0].credentials.machine_id = Some("f".repeat(64));
                        }
                        Ok(healthy_usage())
                    },
                    || now
                )
                .await,
            0
        );
        assert_eq!(manager.available_count(), 0);
    }
}

#[tokio::test]
async fn timeout_and_cancellation_release_gate_and_preserve_cooldown() {
    let manager = Arc::new(manager(1));
    fail_account(&manager, 1);
    let now = due(&manager, 1);
    assert_eq!(
        manager
            .recover_due_accounts_with(|_| pending(), || now)
            .await,
        0
    );
    assert_eq!(manager.entries.lock()[0].recovery_failures, 1);
    assert!(!manager.entries.lock()[0].recovery_probe_in_flight);
    let now = due(&manager, 1);
    let entered = Arc::new(Notify::new());
    let task = tokio::spawn({
        let manager = manager.clone();
        let entered = entered.clone();
        async move {
            manager
                .recover_due_accounts_with(
                    |_| {
                        let entered = entered.clone();
                        async move {
                            entered.notify_one();
                            pending().await
                        }
                    },
                    || now,
                )
                .await
        }
    });
    entered.notified().await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(!manager.entries.lock()[0].recovery_probe_in_flight);
    assert!(due(&manager, 1) > now);
}

#[tokio::test]
async fn probes_are_limited_to_four_per_cycle() {
    let manager = manager(6);
    for id in 1..=6 {
        fail_account(&manager, id);
    }
    let now = due(&manager, 6) + Duration::seconds(1);
    assert_eq!(
        manager
            .recover_due_accounts_with(|_| ready(Ok(healthy_usage())), || now)
            .await,
        4
    );
    assert_eq!(manager.available_count(), 4);
}

#[test]
fn old_disabled_state_starts_persisted_cooldown_and_survives_restart() {
    let dir = TempDirGuard::new(&format!("recovery-migration-{}", uuid::Uuid::new_v4()));
    let path = dir.path().join("credentials.json");
    let mut credential = make_valid_cred("restart");
    credential.id = Some(1);
    std::fs::write(
        dir.path().join("kiro_stats.json"),
        r#"{"1":{"success_count":0,"last_used_at":null,"disabled_reason":"too_many_failures"}}"#,
    )
    .unwrap();
    let first = MultiTokenManager::new(
        config(),
        vec![credential.clone()],
        None,
        Some(path.clone()),
        true,
    )
    .unwrap();
    let next = due(&first, 1);
    assert!(next > Utc::now());
    let second =
        MultiTokenManager::new(config(), vec![credential], None, Some(path), true).unwrap();
    assert_eq!(due(&second, 1), next);
    assert_eq!(second.available_count(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_probe_and_failure_counters_survive_restart() {
    let dir = TempDirGuard::new(&format!("recovery-backoff-{}", uuid::Uuid::new_v4()));
    let path = dir.path().join("credentials.json");
    let first = MultiTokenManager::new(
        config(),
        vec![make_valid_cred("restart")],
        None,
        Some(path.clone()),
        true,
    )
    .unwrap();
    fail_account(&first, 1);
    let now = due(&first, 1);
    first
        .recover_due_accounts_with(
            |_| ready(Err(anyhow::anyhow!("offline failed probe"))),
            || now,
        )
        .await;
    first.persist_credentials().unwrap();
    let credentials: Vec<KiroCredentials> =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let second = MultiTokenManager::new(config(), credentials, None, Some(path), true).unwrap();
    let entry = &second.entries.lock()[0];
    assert!(entry.disabled);
    assert_eq!(entry.failure_count, MAX_FAILURES_PER_CREDENTIAL);
    assert_eq!(entry.recovery_failures, 1);
    assert_eq!(
        entry.recovery_next_probe_at.unwrap(),
        now + Duration::seconds(20)
    );
}
