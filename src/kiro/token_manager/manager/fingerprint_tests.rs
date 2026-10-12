use super::*;
use crate::admin::types::UpdateCredentialRequest;
use crate::model::config::Config;

struct TokenServer {
    url: String,
    proxy_url: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for TokenServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn token_server(rotated_token: &'static str) -> TokenServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = axum::Router::new().route(
        "/token",
        axum::routing::post(move || async move {
            axum::Json(serde_json::json!({
                "access_token": "synthetic-access",
                "refresh_token": rotated_token,
                "expires_in": 3600
            }))
        }),
    );
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    TokenServer {
        url: format!("http://{address}/token"),
        proxy_url: format!("http://{address}"),
        task,
    }
}

struct TempDirectory(std::path::PathBuf);

impl TempDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("kiro-fingerprint-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_import_pins_identity_before_first_refresh_and_persists_it() {
    let directory = TempDirectory::new();
    let path = directory.0.join("credentials.json");
    let server = token_server("rotated-import-token").await;
    let mut config = Config::default();
    config.machine_id = Some("a".repeat(64));
    let imported = KiroCredentials {
        auth_method: Some("external_idp".to_string()),
        client_id: Some("synthetic-client".to_string()),
        refresh_token: Some("original-import-token".to_string()),
        token_endpoint: Some(server.url.clone()),
        // 显式只走本地测试服务，避免系统代理把 loopback 请求转发到外部。
        proxy_url: Some(server.proxy_url.clone()),
        ..Default::default()
    };
    let expected_id = machine_id::generate_from_credentials(&imported, &config);
    let manager =
        MultiTokenManager::new(config.clone(), vec![], None, Some(path.clone()), true).unwrap();
    manager.add_credential(imported).await.unwrap();

    let persisted: Vec<KiroCredentials> =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        persisted[0].machine_id.as_deref(),
        Some(expected_id.as_str())
    );
    assert_eq!(
        persisted[0].refresh_token.as_deref(),
        Some("rotated-import-token")
    );
    let restarted = MultiTokenManager::new(config, persisted, None, Some(path), true).unwrap();
    assert_eq!(
        restarted.entries.lock()[0]
            .credentials
            .machine_id
            .as_deref(),
        Some(expected_id.as_str())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_refresh_token_replacement_preserves_existing_account_identity() {
    let server = token_server("rotated-update-token").await;
    let config = Config::default();
    let original = KiroCredentials {
        auth_method: Some("external_idp".to_string()),
        client_id: Some("synthetic-client".to_string()),
        refresh_token: Some("original-update-token".to_string()),
        token_endpoint: Some(server.url.clone()),
        proxy_url: Some(server.proxy_url.clone()),
        ..Default::default()
    };
    let expected_id = machine_id::generate_from_credentials(&original, &config);
    let manager = MultiTokenManager::new(config, vec![original], None, None, true).unwrap();
    // 模拟旧版 Admin 导入未固定设备标识的在内存账号。
    manager.entries.lock()[0].credentials.machine_id = None;
    manager
        .update_credential(
            1,
            UpdateCredentialRequest {
                refresh_token: Some("replacement-update-token".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let entries = manager.entries.lock();
    assert_eq!(
        entries[0].credentials.machine_id.as_deref(),
        Some(expected_id.as_str())
    );
    assert_eq!(
        entries[0].credentials.refresh_token.as_deref(),
        Some("rotated-update-token")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_explicit_invalid_identity_edit_is_rejected_before_other_mutations() {
    let fixed_id = "b".repeat(64);
    let manager = MultiTokenManager::new(
        Config::default(),
        vec![KiroCredentials {
            machine_id: Some(fixed_id.clone()),
            refresh_token: Some("original-token".to_string()),
            ..Default::default()
        }],
        None,
        None,
        true,
    )
    .unwrap();
    for invalid_id in ["", "bad-id", "gggg"] {
        let error = manager
            .update_credential(
                1,
                UpdateCredentialRequest {
                    machine_id: Some(invalid_id.to_string()),
                    refresh_token: Some("replacement-token".to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("machineId 格式无效"));
        let entries = manager.entries.lock();
        assert_eq!(
            entries[0].credentials.machine_id.as_deref(),
            Some(fixed_id.as_str())
        );
        assert_eq!(
            entries[0].credentials.refresh_token.as_deref(),
            Some("original-token")
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_explicit_valid_identity_edit_is_normalized_and_other_edits_preserve_it() {
    let manager = MultiTokenManager::new(
        Config::default(),
        vec![KiroCredentials {
            machine_id: Some("b".repeat(64)),
            ..Default::default()
        }],
        None,
        None,
        true,
    )
    .unwrap();
    let replacement = "2582956e-cc88-4669-b546-07adbffcb894";
    manager
        .update_credential(
            1,
            UpdateCredentialRequest {
                machine_id: Some(replacement.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    manager
        .update_credential(
            1,
            UpdateCredentialRequest {
                auth_method: Some("idc".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        manager.entries.lock()[0].credentials.machine_id,
        machine_id::normalize_machine_id(replacement)
    );
}

#[test]
fn refresh_commit_never_replaces_original_valid_account_identity() {
    let fixed_id = "B".repeat(64);
    let manager = MultiTokenManager::new(
        Config::default(),
        vec![KiroCredentials {
            machine_id: Some(fixed_id.clone()),
            refresh_token: Some("original-token".to_string()),
            ..Default::default()
        }],
        None,
        None,
        true,
    )
    .unwrap();
    let original = manager.entries.lock()[0].credentials.clone();
    let mut refreshed = original.clone();
    refreshed.refresh_token = Some("rotated-token".to_string());
    refreshed.machine_id = None;

    let committed = manager
        .commit_refreshed_credentials_if_identity_matches(1, &original, &refreshed)
        .unwrap();
    assert_eq!(committed.machine_id.as_deref(), Some(fixed_id.as_str()));
    assert_eq!(committed.refresh_token.as_deref(), Some("rotated-token"));
}
