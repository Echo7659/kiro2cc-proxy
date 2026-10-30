// Copyright (c) 2026 Harllan He. Licensed under MIT.
// 按账号实际可用模型路由（Issue #49）
#[cfg(test)]
mod tests {
    use super::super::super::types::{
        MODEL_SUPPORT_FAILURE_TTL, MODEL_SUPPORT_TTL, ModelSupport, MultiTokenManager,
    };
    use super::super::sticky::tests::make_valid_cred;
    use crate::kiro::model::available_models::{
        AvailableModelInfo, AvailableModelsResponse, TokenLimits,
    };
    use crate::model::config::Config;
    use parking_lot::Mutex;
    use std::collections::HashSet;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};
    use tokio::sync::Semaphore;

    const MODEL: &str = "claude-sonnet-4.6";

    /// 两个账号：#1 优先级更高（priority 模式下默认被选中），#2 为备选
    fn manager_with_two() -> MultiTokenManager {
        let mut a = make_valid_cred("t1");
        a.priority = 0;
        let mut b = make_valid_cred("t2");
        b.priority = 1;
        MultiTokenManager::new(Config::default(), vec![a, b], None, None, false).unwrap()
    }

    fn set_models(m: &MultiTokenManager, id: u64, models: &[&str]) {
        set_support(m, id, Some(models), Instant::now());
    }

    fn set_support(m: &MultiTokenManager, id: u64, models: Option<&[&str]>, at: Instant) {
        let models: Option<HashSet<String>> =
            models.map(|ms| ms.iter().map(|s| s.to_ascii_lowercase()).collect());
        m.model_support.lock().insert(
            id,
            ModelSupport {
                models,
                checked_at: at,
            },
        );
    }

    #[tokio::test]
    async fn skips_account_known_not_to_support_model() {
        let m = manager_with_two();
        // 无探测结果：按优先级选 #1
        assert_eq!(m.acquire_context(Some(MODEL)).await.unwrap().id, 1);

        set_models(&m, 1, &["gpt-5.6-sol"]);
        set_models(&m, 2, &[MODEL]);
        assert_eq!(m.acquire_context(Some(MODEL)).await.unwrap().id, 2);
        // 其他模型不受影响
        assert_eq!(m.acquire_context(Some("gpt-5.6-sol")).await.unwrap().id, 1);
    }

    #[tokio::test]
    async fn model_match_is_case_insensitive() {
        let m = manager_with_two();
        set_models(&m, 1, &["Claude-Sonnet-4.6"]);
        set_models(&m, 2, &["gpt-5.6-sol"]);
        assert_eq!(m.acquire_context(Some(MODEL)).await.unwrap().id, 1);
    }

    #[tokio::test]
    async fn fails_open_when_every_account_is_known_unsupported() {
        // 上游列表可能不完整（例如新模型尚未出现）：全部不支持时保持原有行为
        let m = manager_with_two();
        set_models(&m, 1, &["gpt-5.6-sol"]);
        set_models(&m, 2, &["gpt-5.6-sol"]);
        assert_eq!(m.acquire_context(Some(MODEL)).await.unwrap().id, 1);
    }

    #[tokio::test]
    async fn unknown_status_is_not_excluded() {
        let m = manager_with_two();
        // #1 探测失败（None）、#2 明确支持：#1 仍按优先级参与调度
        set_support(&m, 1, None, Instant::now());
        set_models(&m, 2, &[MODEL]);
        assert_eq!(m.acquire_context(Some(MODEL)).await.unwrap().id, 1);
    }

    #[tokio::test]
    async fn expired_probe_result_is_ignored() {
        let m = manager_with_two();
        let stale = Instant::now()
            .checked_sub(MODEL_SUPPORT_TTL + Duration::from_secs(1))
            .expect("monotonic clock far enough from origin");
        set_support(&m, 1, Some(&["gpt-5.6-sol"]), stale);
        set_models(&m, 2, &[MODEL]);
        // #1 的结果已过期，视为未知，仍按优先级选 #1
        assert_eq!(m.acquire_context(Some(MODEL)).await.unwrap().id, 1);
    }

    #[tokio::test]
    async fn auto_and_missing_model_are_never_filtered() {
        let m = manager_with_two();
        set_models(&m, 1, &["gpt-5.6-sol"]);
        set_models(&m, 2, &["gpt-5.6-sol"]);
        assert_eq!(m.acquire_context(Some("auto")).await.unwrap().id, 1);
        assert_eq!(m.acquire_context(None).await.unwrap().id, 1);
        assert!(!m.is_known_unsupported(1, Some("auto")));
        assert!(!m.is_known_unsupported(1, None));
    }

    #[tokio::test]
    async fn respects_allowed_ids_whitelist() {
        let m = manager_with_two();
        set_models(&m, 1, &["gpt-5.6-sol"]);
        set_models(&m, 2, &[MODEL]);
        // 白名单只含 #1 且其明确不支持：候选全部不支持，保持原行为（不回退到白名单之外）
        let ctx = m
            .acquire_context_sticky(Some(MODEL), &[1], None, &[])
            .await
            .unwrap();
        assert_eq!(ctx.id, 1);
        let ctx = m
            .acquire_context_sticky(Some(MODEL), &[1, 2], None, &[])
            .await
            .unwrap();
        assert_eq!(ctx.id, 2);
    }

    #[tokio::test]
    async fn sticky_binding_to_unsupported_account_is_replaced() {
        let m = manager_with_two();
        // 先让会话绑定到 #1
        let first = m
            .acquire_context_sticky(Some(MODEL), &[], Some("session-x"), &[])
            .await
            .unwrap();
        assert_eq!(first.id, 1);

        // 之后探测到 #1 不支持该模型：会话应改绑到 #2
        set_models(&m, 1, &["gpt-5.6-sol"]);
        set_models(&m, 2, &[MODEL]);
        let second = m
            .acquire_context_sticky(Some(MODEL), &[], Some("session-x"), &[])
            .await
            .unwrap();
        assert_eq!(second.id, 2);
    }

    #[tokio::test]
    async fn balanced_mode_only_rotates_among_supporters() {
        let mut cfg = Config::default();
        cfg.load_balancing_mode = "balanced".to_string();
        let creds = vec![
            make_valid_cred("t1"),
            make_valid_cred("t2"),
            make_valid_cred("t3"),
        ];
        let m = MultiTokenManager::new(cfg, creds, None, None, false).unwrap();
        set_models(&m, 1, &["gpt-5.6-sol"]);
        set_models(&m, 2, &[MODEL]);
        set_models(&m, 3, &[MODEL]);
        for _ in 0..12 {
            let id = m.acquire_context(Some(MODEL)).await.unwrap().id;
            assert_ne!(id, 1, "不支持该模型的账号不应被轮询到");
        }
    }

    #[test]
    fn probe_targets_skip_single_account_pool_and_fresh_results() {
        // 单账号池：没有路由可言，不探测
        let single = MultiTokenManager::new(
            Config::default(),
            vec![make_valid_cred("t1")],
            None,
            None,
            false,
        )
        .unwrap();
        assert!(single.model_support_probe_targets(&[]).is_empty());

        let m = manager_with_two();
        assert_eq!(m.model_support_probe_targets(&[]), vec![1, 2]);

        // 有效的探测结果不重复探测
        set_models(&m, 1, &[MODEL]);
        assert_eq!(m.model_support_probe_targets(&[]), vec![2]);

        // 失败结果在重试间隔后才重新探测
        set_support(&m, 2, None, Instant::now());
        assert!(m.model_support_probe_targets(&[]).is_empty());
        set_support(
            &m,
            2,
            None,
            Instant::now()
                .checked_sub(MODEL_SUPPORT_FAILURE_TTL + Duration::from_secs(1))
                .unwrap(),
        );
        assert_eq!(m.model_support_probe_targets(&[]), vec![2]);
    }

    #[test]
    fn probe_targets_exclude_disabled_accounts_and_honor_whitelist() {
        let m = manager_with_two();
        // 白名单只有 1 个账号：同样没有路由可言
        assert!(m.model_support_probe_targets(&[1]).is_empty());
        // 禁用其中一个后只剩 1 个可调度账号
        m.entries.lock()[1].disabled = true;
        assert!(m.model_support_probe_targets(&[]).is_empty());
    }

    fn resp(ids: &[&str]) -> AvailableModelsResponse {
        AvailableModelsResponse {
            models: ids
                .iter()
                .map(|id| AvailableModelInfo {
                    model_id: id.to_string(),
                    model_name: String::new(),
                    description: None,
                    rate_multiplier: None,
                    token_limits: TokenLimits::default(),
                    additional_model_request_fields_schema: None,
                })
                .collect(),
        }
    }

    const WAIT: Duration = Duration::from_secs(5);

    #[tokio::test]
    async fn refresh_is_noop_for_auto_and_missing_model() {
        // 不会发起任何探测：auto / 无模型直接返回，缓存保持为空
        let m = Arc::new(manager_with_two());
        let calls = Arc::new(AtomicUsize::new(0));
        for model in [Some("auto"), None] {
            let c = calls.clone();
            m.refresh_model_support_with(model, &[], WAIT, move |_, _| {
                c.fetch_add(1, Ordering::SeqCst);
                async { Ok(resp(&[])) }
            })
            .await;
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(m.model_support.lock().is_empty());
    }

    #[tokio::test]
    async fn refresh_records_probe_results() {
        let m = Arc::new(manager_with_two());
        m.refresh_model_support_with(Some(MODEL), &[], WAIT, |_, id| async move {
            if id == 1 {
                Ok(resp(&["GPT-5.6-Sol"]))
            } else {
                Ok(resp(&[MODEL]))
            }
        })
        .await;

        assert!(m.is_known_unsupported(1, Some(MODEL)));
        assert!(!m.is_known_unsupported(2, Some(MODEL)));
        // 探测结果生效后，调度直接排除 #1
        assert_eq!(m.acquire_context(Some(MODEL)).await.unwrap().id, 2);
        // 已新鲜，不再有探测目标
        assert!(m.model_support_probe_targets(&[]).is_empty());
    }

    #[tokio::test]
    async fn empty_list_and_probe_error_are_unknown_with_short_ttl() {
        let m = Arc::new(manager_with_two());
        m.refresh_model_support_with(Some(MODEL), &[], WAIT, |_, id| async move {
            if id == 1 {
                Ok(resp(&[]))
            } else {
                Err(anyhow::anyhow!("boom"))
            }
        })
        .await;

        // 两者都视为未知：不排除
        assert!(!m.is_known_unsupported(1, Some(MODEL)));
        assert!(!m.is_known_unsupported(2, Some(MODEL)));
        let cache = m.model_support.lock();
        assert!(cache.get(&1).unwrap().models.is_none());
        assert!(cache.get(&2).unwrap().models.is_none());
        drop(cache);
        for entry in &m.entries.lock()[..] {
            assert!(!entry.disabled);
            assert_eq!(entry.failure_count, 0);
            assert_eq!(entry.refresh_failure_count, 0);
            assert!(entry.disabled_reason.is_none());
            assert!(entry.recovery_next_probe_at.is_none());
        }
    }

    #[tokio::test]
    async fn concurrent_refreshes_probe_each_account_once() {
        let m = Arc::new(manager_with_two());
        let calls = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let m = m.clone();
            let calls = calls.clone();
            handles.push(tokio::spawn(async move {
                m.refresh_model_support_with(Some(MODEL), &[], WAIT, move |_, _| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    async {
                        tokio::time::sleep(Duration::from_millis(30)).await;
                        Ok(resp(&[MODEL]))
                    }
                })
                .await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        // 2 个账号各探测一次，其余并发请求复用结果
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn slow_probe_does_not_block_request_beyond_max_wait() {
        let m = Arc::new(manager_with_two());
        let started = Instant::now();
        m.refresh_model_support_with(Some(MODEL), &[], Duration::from_millis(30), |_, _| async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            Ok(resp(&["gpt-5.6-sol"]))
        })
        .await;
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "不应等满探测时长"
        );
        // 此刻结果尚未写入：本次调度按未知处理
        assert!(!m.is_known_unsupported(1, Some(MODEL)));

        // 探测没有被取消，在后台完成后写入缓存
        let deadline = Instant::now() + Duration::from_secs(3);
        while !m.is_known_unsupported(1, Some(MODEL)) {
            assert!(Instant::now() < deadline, "后台探测应当完成");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[test]
    fn probe_targets_skip_unhealthy_accounts() {
        let creds = vec![
            make_valid_cred("t1"),
            make_valid_cred("t2"),
            make_valid_cred("t3"),
        ];
        let m = MultiTokenManager::new(Config::default(), creds, None, None, false).unwrap();
        m.entries.lock()[2].failure_count = 3; // Unhealthy
        assert_eq!(m.model_support_probe_targets(&[]), vec![1, 2]);
    }

    #[tokio::test]
    async fn deleting_account_drops_its_probe_result() {
        let m = manager_with_two();
        set_models(&m, 1, &[MODEL]);
        set_models(&m, 2, &[MODEL]);
        m.delete_credential(1).await.unwrap();
        let cache = m.model_support.lock();
        assert!(!cache.contains_key(&1));
        assert!(cache.contains_key(&2));
    }

    #[tokio::test]
    async fn in_flight_probe_cannot_restore_edited_deleted_or_disabled_account_cache() {
        for mutation in ["edit", "delete", "disable"] {
            let m = Arc::new(manager_with_two());
            let started = Arc::new(Semaphore::new(0));
            let release = Arc::new(Semaphore::new(0));
            let task = {
                let m = m.clone();
                let started = started.clone();
                let release = release.clone();
                tokio::spawn(async move {
                    m.refresh_model_support_with(Some(MODEL), &[], WAIT, move |_, id| {
                        let started = started.clone();
                        let release = release.clone();
                        async move {
                            if id == 1 {
                                started.add_permits(1);
                                release.acquire().await.unwrap().forget();
                            }
                            Ok(resp(&["old-principal-model"]))
                        }
                    })
                    .await;
                })
            };
            tokio::time::timeout(WAIT, started.acquire())
                .await
                .expect("first account probe should start")
                .unwrap()
                .forget();

            match mutation {
                "edit" => {
                    let update = serde_json::from_value(serde_json::json!({
                        "machineId": "b".repeat(64)
                    }))
                    .unwrap();
                    m.update_credential(1, update).await.unwrap();
                    // A newer principal's valid cache must not be overwritten by the old probe.
                    set_models(&m, 1, &[MODEL]);
                }
                "delete" => m.delete_credential(1).await.unwrap(),
                "disable" => m.set_disabled(1, true).await.unwrap(),
                _ => unreachable!(),
            }
            release.add_permits(1);
            tokio::time::timeout(WAIT, task)
                .await
                .expect("probe batch should finish")
                .unwrap();

            let cache = m.model_support.lock();
            assert!(cache.contains_key(&2));
            if mutation == "edit" {
                let models = cache.get(&1).unwrap().models.as_ref().unwrap();
                assert!(models.contains(MODEL));
                assert!(!models.contains("old-principal-model"));
            } else {
                assert!(!cache.contains_key(&1), "stale result after {mutation}");
            }
            drop(cache);
            if mutation == "disable" {
                let entries = m.entries.lock();
                let entry = entries.iter().find(|entry| entry.id == 1).unwrap();
                assert!(entry.disabled);
                assert_eq!(
                    entry.disabled_reason,
                    Some(super::super::super::entry::DisabledReason::Manual)
                );
                assert!(entry.recovery_next_probe_at.is_none());
            }
        }
    }

    #[tokio::test]
    async fn probes_are_bounded_and_recheck_queued_admin_changes() {
        let credentials = (1..=9)
            .map(|id| make_valid_cred(&format!("synthetic-{id}")))
            .collect();
        let m = Arc::new(
            MultiTokenManager::new(Config::default(), credentials, None, None, false).unwrap(),
        );
        let started = Arc::new(Semaphore::new(0));
        let release = Arc::new(Semaphore::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let task = {
            let m = m.clone();
            let started = started.clone();
            let release = release.clone();
            let active = active.clone();
            let peak = peak.clone();
            let calls = calls.clone();
            tokio::spawn(async move {
                m.refresh_model_support_with(Some(MODEL), &[], WAIT, move |_, id| {
                    let started = started.clone();
                    let release = release.clone();
                    let active = active.clone();
                    let peak = peak.clone();
                    let calls = calls.clone();
                    async move {
                        let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(count, Ordering::SeqCst);
                        calls.lock().push(id);
                        started.add_permits(1);
                        release.acquire().await.unwrap().forget();
                        active.fetch_sub(1, Ordering::SeqCst);
                        Ok(resp(&[MODEL]))
                    }
                })
                .await;
            })
        };
        tokio::time::timeout(WAIT, started.acquire_many(4))
            .await
            .expect("first four probes should start")
            .unwrap()
            .forget();
        assert_eq!(active.load(Ordering::SeqCst), 4);
        assert_eq!(calls.lock().len(), 4);

        // These entries have tickets but are still waiting for a probe slot.
        m.set_disabled(5, true).await.unwrap();
        m.update_credential(
            6,
            serde_json::from_value(serde_json::json!({"machineId": "c".repeat(64)})).unwrap(),
        )
        .await
        .unwrap();
        m.delete_credential(7).await.unwrap();
        release.add_permits(9);
        tokio::time::timeout(WAIT, task)
            .await
            .expect("remaining eligible probes should finish")
            .unwrap();

        assert_eq!(peak.load(Ordering::SeqCst), 4);
        assert_eq!(active.load(Ordering::SeqCst), 0);
        let mut ids = calls.lock().clone();
        ids.sort_unstable();
        assert_eq!(ids, vec![1, 2, 3, 4, 8, 9]);
        let cache = m.model_support.lock();
        assert!(!cache.contains_key(&5));
        assert!(!cache.contains_key(&6));
        assert!(!cache.contains_key(&7));
    }
}
