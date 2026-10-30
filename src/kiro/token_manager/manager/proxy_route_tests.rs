//! 仅使用本地真实 HTTP/CONNECT 代理线缆的账号绑定测试。
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::provider::KiroProvider;
use crate::kiro::token_manager::types::MultiTokenManager;
use crate::model::config::Config;
use crate::model::proxy_pool::{ProxyPool, ProxyUnavailableError};
use chrono::{Duration, Utc};
use futures::StreamExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct TestProxy {
    port: u16,
    mode: Arc<AtomicU8>,
    health: Arc<AtomicUsize>,
    token: Arc<AtomicUsize>,
    data: Arc<AtomicUsize>,
    connect: Arc<AtomicUsize>,
    authenticated: Arc<AtomicUsize>,
    health_entered: Arc<tokio::sync::Notify>,
    health_release: Arc<tokio::sync::Notify>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for TestProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl TestProxy {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mode = Arc::new(AtomicU8::new(0));
        let health = Arc::new(AtomicUsize::new(0));
        let token = Arc::new(AtomicUsize::new(0));
        let data = Arc::new(AtomicUsize::new(0));
        let connect = Arc::new(AtomicUsize::new(0));
        let authenticated = Arc::new(AtomicUsize::new(0));
        let health_entered = Arc::new(tokio::sync::Notify::new());
        let health_release = Arc::new(tokio::sync::Notify::new());
        let task = tokio::spawn({
            let mode = mode.clone();
            let health = health.clone();
            let token = token.clone();
            let data = data.clone();
            let connect = connect.clone();
            let authenticated = authenticated.clone();
            let health_entered = health_entered.clone();
            let health_release = health_release.clone();
            async move {
                while let Ok((mut socket, _)) = listener.accept().await {
                    let mode = mode.clone();
                    let health = health.clone();
                    let token = token.clone();
                    let data = data.clone();
                    let connect = connect.clone();
                    let authenticated = authenticated.clone();
                    let health_entered = health_entered.clone();
                    let health_release = health_release.clone();
                    tokio::spawn(async move {
                        let mut request = Vec::new();
                        let mut buffer = [0_u8; 2048];
                        while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                            let count = socket.read(&mut buffer).await.unwrap_or(0);
                            if count == 0 || request.len() > 65536 {
                                return;
                            }
                            request.extend_from_slice(&buffer[..count]);
                        }
                        let headers = String::from_utf8_lossy(&request).to_lowercase();
                        let header_end = request
                            .windows(4)
                            .position(|part| part == b"\r\n\r\n")
                            .unwrap()
                            + 4;
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                line.strip_prefix("content-length: ")
                                    .and_then(|value| value.parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        while request.len() < header_end + length {
                            let count = socket.read(&mut buffer).await.unwrap_or(0);
                            if count == 0 {
                                return;
                            }
                            request.extend_from_slice(&buffer[..count]);
                        }
                        if headers.contains("proxy-authorization: basic ") {
                            authenticated.fetch_add(1, Ordering::Relaxed);
                        }
                        let line = headers.lines().next().unwrap_or_default();
                        if line.starts_with("connect ") {
                            connect.fetch_add(1, Ordering::Relaxed);
                            let _ = socket.write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                            return;
                        }
                        let is_health = line.contains("/health");
                        let is_token = line.contains("/token");
                        if is_health {
                            health.fetch_add(1, Ordering::Relaxed);
                        } else if is_token {
                            token.fetch_add(1, Ordering::Relaxed);
                        } else {
                            data.fetch_add(1, Ordering::Relaxed);
                        }
                        let state = mode.load(Ordering::Relaxed);
                        if state == 6 && is_health {
                            health_entered.notify_one();
                            health_release.notified().await;
                            return;
                        }
                        if state == 1 || (state == 2 && is_health) {
                            return;
                        }
                        let (status, body) = if is_health {
                            (200, "198.51.100.20".to_string())
                        } else if is_token {
                            (
                                200,
                                format!(
                                    r#"{{"access_token":"synthetic-{port}","refresh_token":"rotated-synthetic-{port}","expires_in":3600}}"#
                                ),
                            )
                        } else if state == 4 {
                            (403, "provider denied".to_string())
                        } else if state == 5 {
                            (429, "provider limited".to_string())
                        } else {
                            (200, "data".to_string())
                        };
                        if state == 3 && !is_health && !is_token {
                            mode.store(1, Ordering::Relaxed);
                            let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\npartial").await;
                            return;
                        }
                        let reply = format!(
                            "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nRetry-After: 1\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = socket.write_all(reply.as_bytes()).await;
                    });
                }
            }
        });
        Self {
            port,
            mode,
            health,
            token,
            data,
            connect,
            authenticated,
            health_entered,
            health_release,
            task,
        }
    }
    fn import_line(&self) -> String {
        format!("127.0.0.1:{}:synthetic-user:synthetic-password", self.port)
    }
}

struct Fixture {
    directory: std::path::PathBuf,
    config: Config,
    pool: Arc<ProxyPool>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}
impl Fixture {
    async fn new(proxies: &[&TestProxy]) -> Self {
        let directory =
            std::env::temp_dir().join(format!("managed-proxy-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let mut config = Config::default();
        config.proxy_health_check_timeout_secs = 10;
        let mut pool = ProxyPool::load(directory.join("proxies.json"), &config).unwrap();
        pool.set_loopback_health_url("http://127.0.0.1:9/health")
            .unwrap();
        let pool = Arc::new(pool);
        pool.import(
            &proxies
                .iter()
                .map(|proxy| proxy.import_line())
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        pool.check_all().await;
        Self {
            directory,
            config,
            pool,
        }
    }
    fn manager(&self, credentials: Vec<KiroCredentials>) -> Arc<MultiTokenManager> {
        let manager = Arc::new(
            MultiTokenManager::new(
                self.config.clone(),
                credentials,
                None,
                Some(self.directory.join("credentials.json")),
                true,
            )
            .unwrap(),
        );
        manager.attach_proxy_pool(self.pool.clone());
        manager
    }
}
fn credential(name: &str) -> KiroCredentials {
    KiroCredentials {
        access_token: Some(format!("synthetic-{name}")),
        refresh_token: Some(format!("synthetic-{name}-{}", "r".repeat(120))),
        expires_at: Some((Utc::now() + Duration::hours(1)).to_rfc3339()),
        ..Default::default()
    }
}
fn imported(name: &str) -> KiroCredentials {
    KiroCredentials {
        auth_method: Some("external_idp".to_string()),
        client_id: Some("synthetic-client".to_string()),
        token_endpoint: Some("http://127.0.0.1:9/token".to_string()),
        ..credential(name)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn first_validation_is_proxied_and_parallel_imports_prefer_exclusive() {
    let first = TestProxy::start().await;
    let second = TestProxy::start().await;
    let fixture = Fixture::new(&[&first, &second]).await;
    let manager = fixture.manager(vec![]);
    let (one, two) = tokio::join!(
        manager.add_credential(imported("one")),
        manager.add_credential(imported("two"))
    );
    let (one, two) = (one.unwrap(), two.unwrap());
    let entries = manager.entries.lock();
    let a = entries.iter().find(|entry| entry.id == one).unwrap();
    let b = entries.iter().find(|entry| entry.id == two).unwrap();
    assert_ne!(a.credentials.proxy_pool_id, b.credentials.proxy_pool_id);
    assert_eq!(first.token.load(Ordering::Relaxed), 1);
    assert_eq!(second.token.load(Ordering::Relaxed), 1);
    assert!(
        first.authenticated.load(Ordering::Relaxed) > 0
            && second.authenticated.load(Ordering::Relaxed) > 0
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_migration_is_exclusive_then_shared_and_sticky_survives_restart() {
    let first = TestProxy::start().await;
    let second = TestProxy::start().await;
    let fixture = Fixture::new(&[&first, &second]).await;
    let manager = fixture.manager(vec![
        credential("one"),
        credential("two"),
        credential("three"),
    ]);
    let (a, b) = tokio::join!(manager.ensure_proxy_route(1), manager.ensure_proxy_route(2));
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_ne!(a.proxy_pool_id, b.proxy_pool_id);
    let c = manager.ensure_proxy_route(3).await.unwrap();
    assert!(c.proxy_pool_id == a.proxy_pool_id || c.proxy_pool_id == b.proxy_pool_id);
    let machine = a.machine_id.clone();
    let repeated = manager.ensure_proxy_route(1).await.unwrap();
    assert_eq!(repeated.proxy_pool_id, a.proxy_pool_id);
    assert_eq!(repeated.machine_id, machine);
    let credentials = serde_json::from_str(
        &std::fs::read_to_string(fixture.directory.join("credentials.json")).unwrap(),
    )
    .unwrap();
    let reloaded = fixture.manager(credentials);
    let after = reloaded.ensure_proxy_route(1).await.unwrap();
    assert_eq!(after.proxy_pool_id, a.proxy_pool_id);
    assert_eq!(after.machine_id, machine);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configured_unhealthy_pool_fails_closed_without_auth_penalties_or_token_validation() {
    let proxy = TestProxy::start().await;
    proxy.mode.store(1, Ordering::Relaxed);
    let fixture = Fixture::new(&[&proxy]).await;
    let manager = fixture.manager(vec![credential("one")]);
    let error = manager.acquire_context(None).await.err().unwrap();
    assert!(error.downcast_ref::<ProxyUnavailableError>().is_some());
    assert_eq!(manager.entries.lock()[0].refresh_failure_count, 0);
    assert!(!manager.entries.lock()[0].disabled);
    assert!(
        manager
            .add_credential(imported("new"))
            .await
            .err()
            .unwrap()
            .downcast_ref::<ProxyUnavailableError>()
            .is_some()
    );
    assert_eq!(proxy.token.load(Ordering::Relaxed), 0);
    fixture.pool.delete(1).unwrap();
    assert!(fixture.pool.is_configured());
    assert!(manager.acquire_context(None).await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_manual_and_direct_preserved_and_debug_redacts_inline_auth() {
    let proxy = TestProxy::start().await;
    let fixture = Fixture::new(&[&proxy]).await;
    let mut manual = credential("manual");
    manual.proxy_url = Some("http://synthetic-user:synthetic-password@127.0.0.1:88".to_string());
    let mut direct = credential("direct");
    direct.proxy_url = Some("direct".to_string());
    let manager = fixture.manager(vec![manual, direct]);
    let manual = manager.ensure_proxy_route(1).await.unwrap();
    let direct = manager.ensure_proxy_route(2).await.unwrap();
    assert_eq!(manual.proxy_pool_id, None);
    assert_eq!(direct.proxy_pool_id, None);
    assert_eq!(direct.proxy_url.as_deref(), Some("direct"));
    assert_eq!(manual.proxy_username.as_deref(), Some("synthetic-user"));
    assert_eq!(manual.proxy_password.as_deref(), Some("synthetic-password"));
    let debug = format!("{manual:?}");
    assert!(!debug.contains("synthetic-user") && !debug.contains("synthetic-password"));
    let update = serde_json::from_value(serde_json::json!({"proxyUrl":manager.snapshot().entries[0].proxy_url,"proxyUsername":"","proxyPassword":""})).unwrap();
    manager.update_credential(1, update).await.unwrap();
    assert_eq!(
        manager.entries.lock()[0]
            .credentials
            .proxy_password
            .as_deref(),
        Some("synthetic-password")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn managed_blank_editor_keeps_binding_and_manual_edit_detaches() {
    let proxy = TestProxy::start().await;
    let fixture = Fixture::new(&[&proxy]).await;
    let manager = fixture.manager(vec![credential("one")]);
    let before = manager.ensure_proxy_route(1).await.unwrap();
    assert_eq!(manager.snapshot().entries[0].proxy_url, None);
    let update = serde_json::from_value(
        serde_json::json!({"proxyUrl":"","proxyUsername":"","proxyPassword":""}),
    )
    .unwrap();
    manager.update_credential(1, update).await.unwrap();
    let after = manager.ensure_proxy_route(1).await.unwrap();
    assert_eq!(after.proxy_pool_id, before.proxy_pool_id);
    assert_eq!(after.proxy_password, before.proxy_password);
    manager
        .update_credential(
            1,
            serde_json::from_value(
                serde_json::json!({"proxyPassword":"synthetic-manual-password"}),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(manager.entries.lock()[0].credentials.proxy_pool_id, None);
    assert_eq!(
        manager.entries.lock()[0]
            .credentials
            .proxy_password
            .as_deref(),
        Some("synthetic-manual-password")
    );
    manager
        .update_credential(
            1,
            serde_json::from_value(serde_json::json!({"proxyUrl":""})).unwrap(),
        )
        .await
        .unwrap();
    let restored = manager.ensure_proxy_route(1).await.unwrap();
    assert_eq!(restored.proxy_pool_id, before.proxy_pool_id);
    assert_eq!(restored.proxy_password, before.proxy_password);
    manager
        .update_credential(
            1,
            serde_json::from_value(serde_json::json!({"proxyUrl":"direct"})).unwrap(),
        )
        .await
        .unwrap();
    let after = manager.ensure_proxy_route(1).await.unwrap();
    assert_eq!(after.proxy_pool_id, None);
    assert_eq!(after.proxy_url.as_deref(), Some("direct"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_connect_failure_confirms_then_switches_without_changing_auth_or_cooldown() {
    let first = TestProxy::start().await;
    let second = TestProxy::start().await;
    let fixture = Fixture::new(&[&first, &second]).await;
    let manager = fixture.manager(vec![credential("one")]);
    let context = manager.acquire_context(None).await.unwrap();
    let machine = context.credentials.machine_id.clone();
    first.mode.store(2, Ordering::Relaxed);
    let provider = KiroProvider::new(manager.clone());
    let client = provider.client_for(&context.credentials, false).unwrap();
    let result = provider
        .send_admitted(&context, client.get("https://synthetic.invalid/data"))
        .await;
    assert!(result.is_err());
    assert_eq!(first.connect.load(Ordering::Relaxed), 1);
    let after = manager.ensure_proxy_route(1).await.unwrap();
    assert_ne!(after.proxy_pool_id, context.credentials.proxy_pool_id);
    assert_eq!(after.machine_id, machine);
    assert_eq!(manager.entries.lock()[0].failure_count, 0);
    assert!(!manager.entries.lock()[0].disabled);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_403_and_429_never_trigger_proxy_health_failover() {
    for mode in [4, 5] {
        let first = TestProxy::start().await;
        let second = TestProxy::start().await;
        let fixture = Fixture::new(&[&first, &second]).await;
        let manager = fixture.manager(vec![credential("one")]);
        let before = manager.ensure_proxy_route(1).await.unwrap();
        first.mode.store(mode, Ordering::Relaxed);
        let health = first.health.load(Ordering::Relaxed);
        let mut provider = KiroProvider::new(manager.clone());
        provider.upstream_url_override = Some("http://127.0.0.1:9/data".to_string());
        assert!(provider.call_api("{}", false, false, &[]).await.is_err());
        assert_eq!(
            manager.entries.lock()[0].credentials.proxy_pool_id,
            before.proxy_pool_id
        );
        assert_eq!(first.health.load(Ordering::Relaxed), health);
        assert_eq!(second.data.load(Ordering::Relaxed), 0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn emitted_partial_body_is_not_replayed_and_future_request_uses_new_route() {
    let first = TestProxy::start().await;
    let second = TestProxy::start().await;
    let fixture = Fixture::new(&[&first, &second]).await;
    let manager = fixture.manager(vec![credential("one")]);
    let context = manager.acquire_context(None).await.unwrap();
    first.mode.store(3, Ordering::Relaxed);
    let provider = KiroProvider::new(manager.clone());
    let client = provider.client_for(&context.credentials, false).unwrap();
    let response = provider
        .send_admitted(&context, client.get("http://127.0.0.1:9/data"))
        .await
        .unwrap();
    let chunks: Vec<_> = response.bytes_stream().collect().await;
    assert!(
        chunks
            .iter()
            .any(|chunk| chunk.as_ref().is_ok_and(|bytes| !bytes.is_empty()))
    );
    assert!(chunks.iter().any(Result::is_err));
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if manager.entries.lock()[0].credentials.proxy_pool_id
                != context.credentials.proxy_pool_id
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(first.data.load(Ordering::Relaxed), 1);
    assert_eq!(second.data.load(Ordering::Relaxed), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_failure_probe_cannot_override_manual_edit_or_delete() {
    for delete in [false, true] {
        let first = TestProxy::start().await;
        let second = TestProxy::start().await;
        let fixture = Fixture::new(&[&first, &second]).await;
        let manager = fixture.manager(vec![credential("one")]);
        let context = manager.acquire_context(None).await.unwrap();
        first.mode.store(6, Ordering::Relaxed);
        let task = tokio::spawn({
            let manager = manager.clone();
            async move { manager.confirm_proxy_failure(&context).await }
        });
        first.health_entered.notified().await;
        if delete {
            manager.delete_credential(1).await.unwrap();
        } else {
            manager
                .update_credential(
                    1,
                    serde_json::from_value(serde_json::json!({"proxyUrl":"direct"})).unwrap(),
                )
                .await
                .unwrap();
        }
        first.health_release.notify_one();
        assert!(!task.await.unwrap().unwrap());
        if delete {
            assert_eq!(manager.total_count(), 0);
        } else {
            let entries = manager.entries.lock();
            assert_eq!(entries[0].credentials.proxy_pool_id, None);
            assert_eq!(entries[0].credentials.proxy_url.as_deref(), Some("direct"));
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_proxy_switch_preserves_auth_disable_and_recovery_deadline() {
    let first = TestProxy::start().await;
    let second = TestProxy::start().await;
    let fixture = Fixture::new(&[&first, &second]).await;
    let manager = fixture.manager(vec![credential("one")]);
    let context = manager.acquire_context(None).await.unwrap();
    for _ in 0..3 {
        manager.report_failure(1);
    }
    let deadline = manager.entries.lock()[0].recovery_next_probe_at;
    let machine = context.credentials.machine_id;
    fixture
        .pool
        .set_enabled(context.credentials.proxy_pool_id.unwrap(), false)
        .unwrap();
    let after = manager.ensure_proxy_route(1).await.unwrap();
    assert_ne!(after.proxy_pool_id, context.credentials.proxy_pool_id);
    assert_eq!(after.machine_id, machine);
    let entries = manager.entries.lock();
    assert!(entries[0].disabled);
    assert_eq!(entries[0].failure_count, 3);
    assert_eq!(entries[0].recovery_next_probe_at, deadline);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_drop_is_not_a_proxy_failure_or_replay() {
    let first = TestProxy::start().await;
    let second = TestProxy::start().await;
    let fixture = Fixture::new(&[&first, &second]).await;
    let manager = fixture.manager(vec![credential("one")]);
    let context = manager.acquire_context(None).await.unwrap();
    first.mode.store(3, Ordering::Relaxed);
    let health = first.health.load(Ordering::Relaxed);
    let provider = KiroProvider::new(manager.clone());
    let client = provider.client_for(&context.credentials, false).unwrap();
    let response = provider
        .send_admitted(&context, client.get("http://127.0.0.1:9/data"))
        .await
        .unwrap();
    drop(response);
    tokio::task::yield_now().await;
    assert_eq!(
        manager.entries.lock()[0].credentials.proxy_pool_id,
        context.credentials.proxy_pool_id
    );
    assert_eq!(first.health.load(Ordering::Relaxed), health);
    assert_eq!(second.data.load(Ordering::Relaxed), 0);
}
