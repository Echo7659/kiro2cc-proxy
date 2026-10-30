// Copyright (c) 2026 Harllan He. Licensed under MIT.
// token_manager 测试（自 tests.rs 拆出，纯代码搬移）
#[cfg(test)]
pub(crate) mod tests {

    use super::super::super::refresh::{
        UsageLimitsHttpError, extract_profile_arns, is_invalid_grant_response, kam_style_region,
        kiro_ide_user_agents, pick_profile_arn, send_list_profiles, sha256_hex,
        should_try_next_region, validate_refresh_token, with_kiro_ide_headers,
    };
    use super::super::super::types::MultiTokenManager;
    use crate::kiro::token_manager::tests::ext_idp::tests::spawn_single_response_server;

    use crate::kiro::model::credentials::KiroCredentials;

    #[test]
    fn test_validate_refresh_token_missing() {
        let credentials = KiroCredentials::default();
        let result = validate_refresh_token(&credentials);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_refresh_token_valid() {
        let mut credentials = KiroCredentials::default();
        credentials.refresh_token = Some("a".repeat(150));
        let result = validate_refresh_token(&credentials);
        assert!(result.is_ok());
    }

    #[test]
    fn test_sha256_hex() {
        let result = sha256_hex("test");
        assert_eq!(
            result,
            "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
        );
    }

    #[test]
    fn test_is_invalid_grant_response_matches_400_with_expected_body() {
        let body =
            r#"{"error":"invalid_grant","error_description":"Invalid refresh token provided"}"#;
        assert!(is_invalid_grant_response(400, body));
    }

    #[test]
    fn test_is_invalid_grant_response_rejects_other_body() {
        let body = r#"{"error":"server_error","error_description":"something else"}"#;
        assert!(!is_invalid_grant_response(400, body));
    }

    #[test]
    fn test_is_invalid_grant_response_rejects_non_400_status() {
        let body =
            r#"{"error":"invalid_grant","error_description":"Invalid refresh token provided"}"#;
        assert!(!is_invalid_grant_response(401, body));
    }

    #[test]
    fn test_apply_update_fields_clears_subscription_for_identity_updates() {
        let updates = [
            crate::admin::types::UpdateCredentialRequest {
                refresh_token: Some("new-refresh-token".to_string()),
                ..Default::default()
            },
            crate::admin::types::UpdateCredentialRequest {
                auth_method: Some("idc".to_string()),
                ..Default::default()
            },
            crate::admin::types::UpdateCredentialRequest {
                client_id: Some("new-client-id".to_string()),
                ..Default::default()
            },
            crate::admin::types::UpdateCredentialRequest {
                client_secret: Some("new-client-secret".to_string()),
                ..Default::default()
            },
            crate::admin::types::UpdateCredentialRequest {
                profile_arn: Some("new-profile-arn".to_string()),
                ..Default::default()
            },
        ];

        for update in updates {
            let mut credentials = KiroCredentials {
                subscription_title: Some("KIRO PRO".to_string()),
                ..Default::default()
            };
            MultiTokenManager::apply_update_fields(&mut credentials, &update);
            assert_eq!(credentials.subscription_title, None);
        }
    }

    #[test]
    fn test_apply_update_fields_preserves_subscription_for_non_identity_updates() {
        let mut credentials = KiroCredentials {
            subscription_title: Some("KIRO PRO".to_string()),
            ..Default::default()
        };
        let update = crate::admin::types::UpdateCredentialRequest {
            nickname: Some("new-name".to_string()),
            thinking_adaptive: Some(true),
            // 登录来源是纯展示字段，不参与认证流程与订阅等级判定，变更后订阅信息仍然有效
            provider: Some("Google".to_string()),
            ..Default::default()
        };

        MultiTokenManager::apply_update_fields(&mut credentials, &update);

        assert_eq!(credentials.subscription_title.as_deref(), Some("KIRO PRO"));
    }

    #[test]
    fn test_stale_subscription_title_cannot_overwrite_new_identity() {
        let queried_credentials = KiroCredentials {
            refresh_token: Some("old-refresh-token".to_string()),
            subscription_title: None,
            ..Default::default()
        };
        let manager = MultiTokenManager::new(
            crate::model::config::Config::default(),
            vec![queried_credentials.clone()],
            None,
            None,
            false,
        )
        .unwrap();
        manager.entries.lock()[0].credentials.refresh_token = Some("new-refresh-token".to_string());

        let error = manager
            .commit_subscription_title_if_identity_matches(
                1,
                &queried_credentials,
                Some("KIRO PRO"),
            )
            .unwrap_err();

        assert!(error.to_string().contains("身份已变更"));
        assert_eq!(
            manager.entries.lock()[0].credentials.subscription_title,
            None
        );
    }

    #[test]
    fn test_current_subscription_title_is_committed() {
        let initial_credentials = KiroCredentials {
            refresh_token: Some("current-refresh-token".to_string()),
            subscription_title: None,
            ..Default::default()
        };
        let manager = MultiTokenManager::new(
            crate::model::config::Config::default(),
            vec![initial_credentials],
            None,
            None,
            false,
        )
        .unwrap();
        let queried_credentials = manager.entries.lock()[0].credentials.clone();

        let changed = manager
            .commit_subscription_title_if_identity_matches(
                1,
                &queried_credentials,
                Some("KIRO PRO"),
            )
            .unwrap();

        assert!(changed);
        assert_eq!(
            manager.entries.lock()[0]
                .credentials
                .subscription_title
                .as_deref(),
            Some("KIRO PRO")
        );
    }

    #[test]
    fn test_unknown_subscription_clears_stale_pro_title() {
        let initial_credentials = KiroCredentials {
            refresh_token: Some("current-refresh-token".to_string()),
            subscription_title: Some("KIRO PRO".to_string()),
            ..Default::default()
        };
        let manager = MultiTokenManager::new(
            crate::model::config::Config::default(),
            vec![initial_credentials],
            None,
            None,
            false,
        )
        .unwrap();
        let queried_credentials = manager.entries.lock()[0].credentials.clone();

        let changed = manager
            .commit_subscription_title_if_identity_matches(1, &queried_credentials, None)
            .unwrap();

        assert!(changed);
        assert_eq!(
            manager.entries.lock()[0].credentials.subscription_title,
            None
        );
    }

    #[test]
    fn test_stale_token_refresh_cannot_overwrite_new_identity() {
        let initial_credentials = KiroCredentials {
            refresh_token: Some("old-refresh-token".to_string()),
            client_id: Some("old-client-id".to_string()),
            access_token: Some("old-access-token".to_string()),
            ..Default::default()
        };
        let manager = MultiTokenManager::new(
            crate::model::config::Config::default(),
            vec![initial_credentials],
            None,
            None,
            false,
        )
        .unwrap();
        let refreshing_credentials = manager.entries.lock()[0].credentials.clone();
        {
            let mut entries = manager.entries.lock();
            entries[0].credentials.refresh_token = Some("new-refresh-token".to_string());
            entries[0].credentials.client_id = Some("new-client-id".to_string());
            entries[0].credentials.access_token = Some("new-access-token".to_string());
        }
        let mut refreshed_credentials = refreshing_credentials.clone();
        refreshed_credentials.access_token = Some("stale-refreshed-access-token".to_string());
        refreshed_credentials.refresh_token = Some("stale-rotated-refresh-token".to_string());

        let error = manager
            .commit_refreshed_credentials_if_identity_matches(
                1,
                &refreshing_credentials,
                &refreshed_credentials,
            )
            .unwrap_err();

        assert!(error.to_string().contains("身份已变更"));
        let credentials = &manager.entries.lock()[0].credentials;
        assert_eq!(
            credentials.refresh_token.as_deref(),
            Some("new-refresh-token")
        );
        assert_eq!(credentials.client_id.as_deref(), Some("new-client-id"));
        assert_eq!(
            credentials.access_token.as_deref(),
            Some("new-access-token")
        );
    }

    #[test]
    fn test_token_refresh_preserves_concurrent_non_identity_updates() {
        let initial_credentials = KiroCredentials {
            refresh_token: Some("current-refresh-token".to_string()),
            access_token: Some("old-access-token".to_string()),
            nickname: Some("old-name".to_string()),
            subscription_title: Some("KIRO PRO".to_string()),
            ..Default::default()
        };
        let manager = MultiTokenManager::new(
            crate::model::config::Config::default(),
            vec![initial_credentials],
            None,
            None,
            false,
        )
        .unwrap();
        let refreshing_credentials = manager.entries.lock()[0].credentials.clone();
        {
            let mut entries = manager.entries.lock();
            let credentials = &mut entries[0].credentials;
            credentials.nickname = Some("new-name".to_string());
            credentials.email = Some("fictional@example.invalid".to_string());
            credentials.proxy_url = Some("direct".to_string());
            credentials.priority = 7;
            credentials.thinking_adaptive = true;
            credentials.subscription_title = Some("KIRO FREE".to_string());
        }
        let mut refreshed_credentials = refreshing_credentials.clone();
        refreshed_credentials.access_token = Some("refreshed-access-token".to_string());
        refreshed_credentials.sso_access_token = Some("refreshed-sso-token".to_string());
        refreshed_credentials.refresh_token = Some("rotated-refresh-token".to_string());
        refreshed_credentials.profile_arn = Some("refreshed-profile-arn".to_string());
        refreshed_credentials.expires_at = Some("2099-01-01T00:00:00Z".to_string());

        let committed = manager
            .commit_refreshed_credentials_if_identity_matches(
                1,
                &refreshing_credentials,
                &refreshed_credentials,
            )
            .unwrap();

        assert_eq!(
            committed.access_token.as_deref(),
            Some("refreshed-access-token")
        );
        assert_eq!(
            committed.sso_access_token.as_deref(),
            Some("refreshed-sso-token")
        );
        assert_eq!(
            committed.refresh_token.as_deref(),
            Some("rotated-refresh-token")
        );
        assert_eq!(
            committed.profile_arn.as_deref(),
            Some("refreshed-profile-arn")
        );
        assert_eq!(
            committed.expires_at.as_deref(),
            Some("2099-01-01T00:00:00Z")
        );
        assert_eq!(committed.nickname.as_deref(), Some("new-name"));
        assert_eq!(
            committed.email.as_deref(),
            Some("fictional@example.invalid")
        );
        assert_eq!(committed.proxy_url.as_deref(), Some("direct"));
        assert_eq!(committed.priority, 7);
        assert!(committed.thinking_adaptive);
        assert_eq!(committed.subscription_title.as_deref(), Some("KIRO FREE"));
    }

    #[tokio::test]
    async fn test_identity_update_waits_for_refresh_identity_lock() {
        let manager = std::sync::Arc::new(
            MultiTokenManager::new(
                crate::model::config::Config::default(),
                vec![KiroCredentials {
                    refresh_token: Some("current-refresh-token".to_string()),
                    machine_id: Some("a".repeat(64)),
                    ..Default::default()
                }],
                None,
                None,
                false,
            )
            .unwrap(),
        );
        let identity_guard = manager.credential_identity_lock.lock().await;
        let update_manager = std::sync::Arc::clone(&manager);
        let update_task = tokio::spawn(async move {
            update_manager
                .update_credential(
                    1,
                    crate::admin::types::UpdateCredentialRequest {
                        machine_id: Some("b".repeat(64)),
                        ..Default::default()
                    },
                )
                .await
        });

        tokio::task::yield_now().await;
        assert!(!update_task.is_finished());

        drop(identity_guard);
        tokio::time::timeout(std::time::Duration::from_secs(1), update_task)
            .await
            .expect("身份锁释放后更新应及时完成")
            .expect("更新任务不应 panic")
            .expect("身份更新应成功");
        assert_eq!(
            manager.entries.lock()[0].credentials.machine_id.as_deref(),
            Some("b".repeat(64).as_str())
        );
    }

    #[tokio::test]
    async fn test_discovered_profile_arn_commit_rejects_stale_identity() {
        // 写回 profileArn 仅在账号身份与查询时快照一致时生效；
        // 查询期间 refreshToken 被更换（身份已变更）时必须拒绝脏写
        let manager = std::sync::Arc::new(
            MultiTokenManager::new(
                crate::model::config::Config::default(),
                vec![KiroCredentials {
                    refresh_token: Some("current-refresh-token".to_string()),
                    ..Default::default()
                }],
                None,
                None,
                false,
            )
            .unwrap(),
        );
        let credentials = manager.entries.lock()[0].credentials.clone();

        // 1) 身份一致 → 写回成功
        manager
            .commit_discovered_profile_arn(
                1,
                &credentials,
                "arn:aws:codewhisperer:us-east-1:111122223333:profile/ABCDEFGHIJKL",
            )
            .expect("身份一致时写回应成功");
        assert_eq!(
            manager.entries.lock()[0].credentials.profile_arn.as_deref(),
            Some("arn:aws:codewhisperer:us-east-1:111122223333:profile/ABCDEFGHIJKL")
        );

        // 2) 查询快照过期（refreshToken 已更换）→ 拒绝写回，内存中 ARN 不变
        let mut stale = credentials.clone();
        stale.refresh_token = Some("rotated-refresh-token".to_string());
        let err = manager
            .commit_discovered_profile_arn(
                1,
                &stale,
                "arn:aws:codewhisperer:us-east-1:111122223333:profile/ZZZZZZZZZZZZ",
            )
            .expect_err("身份已变更时写回应被拒绝");
        assert!(err.to_string().contains("身份已变更"));
        assert_eq!(
            manager.entries.lock()[0].credentials.profile_arn.as_deref(),
            Some("arn:aws:codewhisperer:us-east-1:111122223333:profile/ABCDEFGHIJKL"),
            "拒绝后不应覆盖已写回的 ARN"
        );

        // 3) 账号不存在 → 报错
        assert!(
            manager
                .commit_discovered_profile_arn(
                    999,
                    &credentials,
                    "arn:aws:codewhisperer:us-east-1:111122223333:profile/AAAAAAAAAAAA",
                )
                .is_err()
        );
    }

    #[test]
    fn test_apply_update_fields_thinking_adaptive() {
        let mut cred = KiroCredentials::default();
        assert!(!cred.thinking_adaptive);

        // Some(v) 覆盖现有值
        let mut update = crate::admin::types::UpdateCredentialRequest {
            thinking_adaptive: Some(true),
            ..Default::default()
        };
        MultiTokenManager::apply_update_fields(&mut cred, &update);
        assert!(cred.thinking_adaptive);

        update.thinking_adaptive = Some(false);
        MultiTokenManager::apply_update_fields(&mut cred, &update);
        assert!(!cred.thinking_adaptive);

        // None 表示不更新该字段
        let update = crate::admin::types::UpdateCredentialRequest {
            thinking_adaptive: None,
            ..Default::default()
        };
        cred.thinking_adaptive = true;
        MultiTokenManager::apply_update_fields(&mut cred, &update);
        assert!(cred.thinking_adaptive);
    }

    #[test]
    fn test_apply_update_fields_provider() {
        let mut cred = KiroCredentials::default();

        // Some(非空) 写入登录来源
        let update = crate::admin::types::UpdateCredentialRequest {
            provider: Some("Google".to_string()),
            ..Default::default()
        };
        MultiTokenManager::apply_update_fields(&mut cred, &update);
        assert_eq!(cred.provider.as_deref(), Some("Google"));

        // 空字符串清除该字段
        let update = crate::admin::types::UpdateCredentialRequest {
            provider: Some(String::new()),
            ..Default::default()
        };
        MultiTokenManager::apply_update_fields(&mut cred, &update);
        assert_eq!(cred.provider, None);

        // None 表示不更新该字段
        cred.provider = Some("GitHub".to_string());
        let update = crate::admin::types::UpdateCredentialRequest {
            provider: None,
            ..Default::default()
        };
        MultiTokenManager::apply_update_fields(&mut cred, &update);
        assert_eq!(cred.provider.as_deref(), Some("GitHub"));
    }

    #[test]
    fn test_extract_profile_arns_supports_arn_and_profile_arn_fields() {
        let v = serde_json::json!({
            "profiles": [
                {"arn": "arn:aws:codewhisperer:us-east-1:111:profile/A", "profileName": "a"},
                {"profileArn": "arn:aws:codewhisperer:eu-central-1:111:profile/B"},
                {"arn": ""},
                {"name": "no-arn"}
            ]
        });
        assert_eq!(
            extract_profile_arns(&v),
            vec![
                "arn:aws:codewhisperer:us-east-1:111:profile/A".to_string(),
                "arn:aws:codewhisperer:eu-central-1:111:profile/B".to_string(),
            ]
        );
        assert!(extract_profile_arns(&serde_json::json!({})).is_empty());
        assert!(extract_profile_arns(&serde_json::json!({"profiles": null})).is_empty());
    }

    #[test]
    fn test_pick_profile_arn_prefers_region_match_then_first() {
        let arns = vec![
            "arn:aws:codewhisperer:us-east-1:111:profile/A".to_string(),
            "arn:aws:codewhisperer:eu-central-1:111:profile/B".to_string(),
        ];
        assert_eq!(
            pick_profile_arn(&arns, "eu-central-1").as_deref(),
            Some("arn:aws:codewhisperer:eu-central-1:111:profile/B")
        );
        // 无区域匹配 → 第一个
        assert_eq!(
            pick_profile_arn(&arns, "ap-southeast-1").as_deref(),
            Some("arn:aws:codewhisperer:us-east-1:111:profile/A")
        );
        assert_eq!(pick_profile_arn(&[], "us-east-1"), None);
    }

    #[test]
    fn test_kam_style_region_maps_to_supported_control_plane_regions() {
        assert_eq!(kam_style_region("us-east-1"), "us-east-1");
        assert_eq!(kam_style_region("us-west-2"), "us-east-1");
        assert_eq!(kam_style_region("eu-central-1"), "eu-central-1");
        assert_eq!(kam_style_region("EU-WEST-1"), "eu-central-1");
    }

    #[test]
    fn test_kiro_ide_headers_are_identical_for_both_list_profiles_shapes() {
        // ListAvailableProfiles 的 REST / JSON-RPC 两种形态必须带同一套 Kiro IDE 请求头，
        // 否则一种形态被上游按指纹拦截时无法与另一种对比排查
        let credentials = KiroCredentials::default();
        let config = crate::model::config::Config::default();
        let agents = kiro_ide_user_agents(&credentials, &config);
        assert!(agents.user_agent.contains("KiroIDE-"));
        assert!(agents.amz_user_agent.contains("KiroIDE-"));

        let client = reqwest::Client::new();
        for url in [
            "https://example.com/ListAvailableProfiles",
            "https://example.com/",
        ] {
            let req = with_kiro_ide_headers(client.post(url), &agents)
                .build()
                .unwrap();
            let h = req.headers();
            assert_eq!(h["user-agent"].to_str().unwrap(), agents.user_agent);
            assert_eq!(
                h["x-amz-user-agent"].to_str().unwrap(),
                agents.amz_user_agent
            );
            assert!(h.contains_key("amz-sdk-invocation-id"));
            assert_eq!(h["amz-sdk-request"], "attempt=1; max=1");
        }
    }

    #[tokio::test]
    async fn test_send_list_profiles_parses_arns_on_success() {
        let url = spawn_single_response_server(
            200,
            r#"{"profiles":[{"arn":"arn:aws:codewhisperer:us-east-1:111122223333:profile/ABCDEFGHIJKL"}]}"#,
        )
        .await;
        let arns = send_list_profiles(reqwest::Client::new().post(url))
            .await
            .unwrap();
        assert_eq!(
            arns,
            vec!["arn:aws:codewhisperer:us-east-1:111122223333:profile/ABCDEFGHIJKL"]
        );
    }

    #[tokio::test]
    async fn test_send_list_profiles_reports_status_on_http_error() {
        let url = spawn_single_response_server(403, r#"{"message":"denied"}"#).await;
        let err = send_list_profiles(reqwest::Client::new().post(url))
            .await
            .unwrap_err();
        assert!(err.contains("403") && err.contains("denied"), "实际: {err}");
    }

    #[tokio::test]
    async fn test_send_list_profiles_reports_unparseable_body_as_error() {
        // 2xx 但响应体不是 JSON：应作为可重试的 Err 返回，让调用方换另一种请求形态
        let url = spawn_single_response_server(200, "not json").await;
        let err = send_list_profiles(reqwest::Client::new().post(url))
            .await
            .unwrap_err();
        assert!(err.contains("响应解析失败"), "实际: {err}");
    }

    fn http_error(code: u16, body: &str) -> anyhow::Error {
        UsageLimitsHttpError::from_response(
            reqwest::StatusCode::from_u16(code).unwrap(),
            body.to_string(),
        )
        .into()
    }

    #[test]
    fn test_usage_limits_http_error_display_keeps_legacy_message_format() {
        // admin/service.rs 的错误分类依赖这些关键字，格式变动会让其静默失效
        assert_eq!(
            http_error(403, "denied").to_string(),
            "权限不足，无法获取使用额度: 403 Forbidden denied"
        );
        assert!(
            http_error(401, "")
                .to_string()
                .starts_with("认证失败，Token 无效或已过期: 401")
        );
        assert!(
            http_error(429, "")
                .to_string()
                .starts_with("请求过于频繁，已被限流: 429")
        );
        assert!(
            http_error(503, "")
                .to_string()
                .starts_with("服务器错误，AWS 服务暂时不可用: 503")
        );
        assert!(
            http_error(418, "")
                .to_string()
                .starts_with("获取使用额度失败: 418")
        );
    }

    #[test]
    fn test_should_try_next_region_only_on_403_status() {
        assert!(should_try_next_region(&http_error(403, "")));
        for code in [400, 401, 429, 500, 503] {
            assert!(!should_try_next_region(&http_error(code, "")), "{code}");
        }
        // 非 HTTP 状态错误（网络等）不换区域；不依赖文案：含“权限不足”字样的普通错误同样不触发
        assert!(!should_try_next_region(&anyhow::anyhow!("权限不足")));
    }

    #[test]
    fn test_update_requires_revalidation_covers_client_credentials() {
        use crate::admin::types::UpdateCredentialRequest as Req;
        let requires = MultiTokenManager::update_requires_revalidation;
        let with_method = |m: &str| KiroCredentials {
            auth_method: Some(m.to_string()),
            ..Default::default()
        };
        let idc = with_method("idc");
        let ext = with_method("external_idp");
        let social = with_method("social");
        let secret = Req {
            client_secret: Some("secret".into()),
            ..Default::default()
        };

        assert!(requires(
            &Req {
                refresh_token: Some("rt".into()),
                ..Default::default()
            },
            &social
        ));
        // issue #58：IdC 账号只改 clientSecret 也要先试刷新，错误值不能静默落盘
        assert!(requires(&secret, &idc));
        // clientSecret 只在 IdC 刷新里参与请求：external_idp / social 试刷新验证不到它
        assert!(!requires(&secret, &ext));
        assert!(!requires(&secret, &social));
        // authMethod 省略时按 clientId + clientSecret 是否齐全推断（与 refresh_token 一致）
        assert!(requires(
            &secret,
            &KiroCredentials {
                client_id: Some("id".into()),
                client_secret: Some("secret".into()),
                ..Default::default()
            }
        ));
        // clientId 对 IdC / external_idp 都参与请求
        let id = Req {
            client_id: Some("id".into()),
            ..Default::default()
        };
        assert!(requires(&id, &idc));
        assert!(requires(&id, &ext));
        // 空串是清除，不涉及新凭据；其他字段更新同样不验证
        assert!(!requires(
            &Req {
                client_secret: Some(String::new()),
                client_id: Some(String::new()),
                ..Default::default()
            },
            &idc
        ));
        assert!(!requires(
            &Req {
                nickname: Some("n".into()),
                proxy_password: Some("p".into()),
                ..Default::default()
            },
            &idc
        ));
    }

    fn external_idp_manager(token_endpoint: String) -> MultiTokenManager {
        MultiTokenManager::new(
            crate::model::config::Config::default(),
            vec![KiroCredentials {
                auth_method: Some("external_idp".to_string()),
                refresh_token: Some("short-refresh-token".to_string()),
                client_id: Some("client-id".to_string()),
                client_secret: Some("real-secret".to_string()),
                token_endpoint: Some(token_endpoint),
                ..Default::default()
            }],
            None,
            None,
            false,
        )
        .unwrap()
    }

    /// 覆盖边界：本用例走 external_idp（可用本地假 token 端点），它的刷新请求带 clientId 但
    /// 不带 clientSecret，所以这里验证的是「试刷新失败 → 拒绝写入且原值不变」这条链路本身。
    /// IdC 路径（clientSecret 真正生效的那条）的端点硬编码为 AWS OIDC 地址，无法本地模拟，
    /// 「新 clientSecret 被上游校验」这一点没有自动化测试覆盖。
    #[tokio::test]
    async fn test_update_client_id_rejected_when_trial_refresh_fails() {
        let body = r#"{"error":"invalid_client","error_description":"Invalid client provided"}"#;
        let endpoint = spawn_single_response_server(401, body).await;
        let manager = external_idp_manager(endpoint);
        let cooldown = chrono::Utc::now() + chrono::Duration::minutes(10);
        let (original, generation) = {
            let mut entries = manager.entries.lock();
            let entry = &mut entries[0];
            // A still-valid access token must not bypass validation of replacement clientId.
            entry.credentials.access_token = Some("synthetic-still-valid-token".into());
            entry.credentials.expires_at =
                Some((chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339());
            entry.credentials.proxy_url = Some("direct".into());
            entry.credentials.disabled = true;
            entry.disabled = true;
            entry.disabled_reason = Some(crate::kiro::token_manager::DisabledReason::QuotaExceeded);
            entry.failure_count = 1;
            entry.refresh_failure_count = 2;
            entry.recovery_failures = 3;
            entry.recovery_next_probe_at = Some(cooldown);
            (
                serde_json::to_value(&entry.credentials).unwrap(),
                entry.recovery_generation,
            )
        };

        let err = manager
            .update_credential(
                1,
                crate::admin::types::UpdateCredentialRequest {
                    client_id: Some("wrong-client-id".to_string()),
                    machine_id: Some("b".repeat(64)),
                    ..Default::default()
                },
            )
            .await
            .expect_err("新 clientId 试刷新失败时应拒绝更新");
        assert!(
            err.to_string().contains("401") && err.to_string().contains("invalid_client"),
            "应保留上游状态码与原文便于排查，实际: {err}"
        );
        assert_eq!(
            manager.entries.lock()[0].credentials.client_id.as_deref(),
            Some("client-id"),
            "验证失败不应改动内存中的凭据"
        );
        let entries = manager.entries.lock();
        let entry = &entries[0];
        assert_eq!(serde_json::to_value(&entry.credentials).unwrap(), original);
        assert_eq!(entry.recovery_generation, generation);
        assert_eq!(entry.recovery_next_probe_at, Some(cooldown));
        assert_eq!(entry.recovery_failures, 3);
        assert_eq!(entry.failure_count, 1);
        assert_eq!(entry.refresh_failure_count, 2);
        assert!(entry.disabled);
        assert_eq!(
            entry.disabled_reason,
            Some(crate::kiro::token_manager::DisabledReason::QuotaExceeded)
        );
    }

    #[tokio::test]
    async fn test_client_id_trial_refresh_pins_machine_id_and_preserves_manual_disable() {
        let endpoint = spawn_single_response_server(
            200,
            r#"{"access_token":"synthetic-new-access","refresh_token":"synthetic-rotated-refresh","expires_in":3600}"#,
        )
        .await;
        let manager = external_idp_manager(endpoint);
        let original_machine_id = {
            let mut entries = manager.entries.lock();
            let entry = &mut entries[0];
            entry.credentials.proxy_url = Some("direct".into());
            entry.credentials.disabled = true;
            entry.disabled = true;
            entry.disabled_reason = Some(crate::kiro::token_manager::DisabledReason::Manual);
            entry.credentials.machine_id.clone()
        };
        manager
            .update_credential(
                1,
                crate::admin::types::UpdateCredentialRequest {
                    client_id: Some("synthetic-new-client".into()),
                    // Blank managed/manual form defaults must not introduce a new proxy route.
                    proxy_url: Some("direct".into()),
                    proxy_username: Some(String::new()),
                    proxy_password: Some(String::new()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let entries = manager.entries.lock();
        let entry = &entries[0];
        assert_eq!(
            entry.credentials.client_id.as_deref(),
            Some("synthetic-new-client")
        );
        assert_eq!(
            entry.credentials.access_token.as_deref(),
            Some("synthetic-new-access")
        );
        assert_eq!(
            entry.credentials.refresh_token.as_deref(),
            Some("synthetic-rotated-refresh")
        );
        assert_eq!(entry.credentials.machine_id, original_machine_id);
        assert_eq!(entry.credentials.proxy_url.as_deref(), Some("direct"));
        assert!(entry.credentials.proxy_pool_id.is_none());
        assert!(entry.credentials.disabled && entry.disabled);
        assert_eq!(
            entry.disabled_reason,
            Some(crate::kiro::token_manager::DisabledReason::Manual)
        );
        assert!(entry.recovery_next_probe_at.is_none());
        assert_eq!(entry.recovery_generation, 1);
    }

    #[tokio::test]
    async fn test_update_client_secret_skips_trial_refresh_for_non_idc() {
        // external_idp 不使用 clientSecret：token 端点不可达也应直接成功，不做空转的试刷新
        let manager = external_idp_manager("http://127.0.0.1:1/token".to_string());

        manager
            .update_credential(
                1,
                crate::admin::types::UpdateCredentialRequest {
                    client_secret: Some("new-secret".to_string()),
                    ..Default::default()
                },
            )
            .await
            .expect("非 IdC 账号改 clientSecret 不需要试刷新");
        assert_eq!(
            manager.entries.lock()[0]
                .credentials
                .client_secret
                .as_deref(),
            Some("new-secret")
        );
    }

    #[tokio::test]
    async fn test_trial_refresh_uses_proxy_from_same_update() {
        // 试刷新必须按「更新后」的配置发起：同一请求里把账号代理换成不可达地址时，
        // 验证应走新代理并失败，而不是借旧的直连通过后把一个不可用的配置落盘。
        let body = r#"{"access_token":"new-access-token","expires_in":3600}"#;
        let endpoint = spawn_single_response_server(200, body).await;
        let manager = external_idp_manager(endpoint);

        manager
            .update_credential(
                1,
                crate::admin::types::UpdateCredentialRequest {
                    client_id: Some("new-client-id".to_string()),
                    proxy_url: Some("http://127.0.0.1:1".to_string()),
                    ..Default::default()
                },
            )
            .await
            .expect_err("新代理不可达时试刷新应失败");
        let entries = manager.entries.lock();
        assert_eq!(entries[0].credentials.proxy_url, None, "失败不应落盘新代理");
        assert_eq!(
            entries[0].credentials.client_id.as_deref(),
            Some("client-id")
        );
    }

    #[tokio::test]
    async fn test_update_clearing_client_secret_skips_trial_refresh() {
        // 清除（空串）与无关字段更新不触发试刷新：token 端点不可达也应成功
        let manager = external_idp_manager("http://127.0.0.1:1/token".to_string());

        manager
            .update_credential(
                1,
                crate::admin::types::UpdateCredentialRequest {
                    client_secret: Some(String::new()),
                    nickname: Some("renamed".to_string()),
                    ..Default::default()
                },
            )
            .await
            .expect("清除 clientSecret 不需要验证");
        let entries = manager.entries.lock();
        assert_eq!(entries[0].credentials.client_secret, None);
        assert_eq!(entries[0].credentials.nickname.as_deref(), Some("renamed"));
    }

    #[test]
    fn test_updated_field_names_lists_names_only() {
        let names =
            MultiTokenManager::updated_field_names(&crate::admin::types::UpdateCredentialRequest {
                client_secret: Some("TOP-SECRET-VALUE".to_string()),
                nickname: Some("n".to_string()),
                ..Default::default()
            });
        assert_eq!(names, vec!["clientSecret", "nickname"]);
    }
}
