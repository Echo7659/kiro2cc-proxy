use super::*;
use std::sync::atomic::AtomicUsize;
use tokio::sync::Notify;

const INPUT: &str = "gateway.example:10001:demo-user:demo~password:colon";
const PASSWORD: &str = "demo~password:colon";

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("kiro-proxy-pool-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> PathBuf {
        self.0.join("proxies.json")
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn pool() -> (Arc<ProxyPool>, Directory) {
    let directory = Directory::new();
    let pool = ProxyPool::load(directory.path(), &Config::default()).unwrap();
    (Arc::new(pool), directory)
}

fn healthy(pool: &ProxyPool) {
    for entry in &mut pool.inner.lock().entries {
        entry.record.status = ProxyStatus::Healthy;
        entry.record.cooldown_until = None;
    }
}

#[test]
fn parser_preserves_password_colons_tildes_and_percent_encoded_authentication() {
    let parsed = parser::parse(INPUT).unwrap();
    assert_eq!(parsed.url, "http://gateway.example:10001");
    assert_eq!(parsed.username.as_deref(), Some("demo-user"));
    assert_eq!(parsed.password.as_deref(), Some(PASSWORD));
    let encoded =
        parser::parse("http://demo%2Duser:demo%7Epassword%3Acolon@GATEWAY.EXAMPLE:10001/").unwrap();
    assert_eq!(encoded.url, parsed.url);
    assert_eq!(encoded.username, parsed.username);
    assert_eq!(encoded.password, parsed.password);
    let ipv6 = parser::parse("[::1]:10001:demo-user:demo~password:colon").unwrap();
    assert_eq!(ipv6.url, "http://[::1]:10001");
    for scheme in ["https", "socks5", "socks5h"] {
        assert!(parser::parse(&format!("{scheme}://gateway.example:10001")).is_ok());
    }
}

#[test]
fn invalid_input_errors_are_static_and_health_target_requires_https() {
    for input in [
        "ftp://demo-user:demo~password:colon@gateway.example:10001",
        "http://demo-user:demo~password:colon@gateway.example:10001/private",
        "gateway.example:0:demo-user:demo~password:colon",
        "http://gateway.example:10001?password=demo~password:colon",
        "gateway.example:10001:demo-user",
    ] {
        let error = parser::parse(input).err().unwrap();
        assert!(!error.contains(PASSWORD));
        assert!(!error.contains("demo-user"));
    }
    for target in [
        "http://checkip.amazonaws.com",
        "https://demo-user:demo~password:colon@checkip.amazonaws.com/",
        "https://checkip.amazonaws.com/?x=1",
    ] {
        assert!(parser::validate_health_url(target).is_err());
    }
}

#[test]
fn import_deduplicates_normalized_routes_and_never_exports_credentials() {
    let (pool, _directory) = pool();
    let result = pool.import(&format!("{INPUT}\nhttp://demo-user:demo%7Epassword%3Acolon@GATEWAY.EXAMPLE:10001\nftp://demo-user:demo~password:colon@gateway.example\n")).unwrap();
    assert_eq!(
        (result.imported, result.duplicates, result.rejected),
        (1, 1, 1)
    );
    assert_eq!(result.errors[0].line, 3);
    for encoded in [
        serde_json::to_string(&pool.summaries()).unwrap(),
        serde_json::to_string(&result).unwrap(),
        format!("{:?}", pool.route(1).unwrap()),
    ] {
        assert!(!encoded.contains(PASSWORD));
        assert!(!encoded.contains("demo-user"));
        assert!(!encoded.contains("@gateway"));
    }
}

#[test]
fn absent_storage_stays_legacy_and_delete_all_preserves_configured_flag_and_ids() {
    let (pool, directory) = pool();
    assert!(!pool.is_configured());
    assert!(!directory.path().exists());
    pool.import(INPUT).unwrap();
    pool.delete(1).unwrap();
    assert!(pool.is_configured());
    let restored = ProxyPool::load(directory.path(), &Config::default()).unwrap();
    assert!(restored.is_configured());
    assert!(restored.summaries().is_empty());
    restored.import(INPUT).unwrap();
    assert_eq!(restored.summaries()[0].id, 2);
}

#[test]
fn private_storage_has_mode_600_and_keeps_authentication_separate_from_url() {
    let (pool, directory) = pool();
    pool.import(INPUT).unwrap();
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(directory.path()).unwrap()).unwrap();
    assert_eq!(value["proxies"][0]["url"], "http://gateway.example:10001");
    assert_eq!(value["proxies"][0]["password"], PASSWORD);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(directory.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    healthy(&pool);
    storage::save(&directory.path(), &pool.inner.lock()).unwrap();
    let restored = ProxyPool::load(directory.path(), &Config::default()).unwrap();
    assert!(restored.get_healthy_route(1).is_none());
    assert_eq!(
        restored.route(1).unwrap().password.as_deref(),
        Some(PASSWORD)
    );
}

#[test]
fn least_assigned_selection_prefers_exclusive_then_shared_without_rebalancing() {
    let (pool, _directory) = pool();
    pool.import(&format!(
        "{INPUT}\ngateway.example:10002:demo-user:{PASSWORD}"
    ))
    .unwrap();
    assert!(pool.select_healthy_proxy(&HashMap::new(), &[]).is_none());
    healthy(&pool);
    assert_eq!(
        pool.select_healthy_proxy(&HashMap::new(), &[]).unwrap().id,
        1
    );
    assert_eq!(
        pool.select_healthy_proxy(&HashMap::from([(1, 1)]), &[])
            .unwrap()
            .id,
        2
    );
    assert_eq!(
        pool.select_healthy_proxy(&HashMap::from([(1, 3), (2, 1)]), &[])
            .unwrap()
            .id,
        2
    );
    assert_eq!(
        pool.select_healthy_proxy(&HashMap::new(), &[1]).unwrap().id,
        2
    );
    pool.set_enabled(2, false).unwrap();
    assert!(pool.select_healthy_proxy(&HashMap::new(), &[1]).is_none());
}

#[test]
fn failed_persistence_rolls_back_import_and_configured_flag() {
    let directory = Directory::new();
    let invalid_parent = directory.0.join("plain-file");
    std::fs::write(&invalid_parent, "test").unwrap();
    let pool = ProxyPool::load(invalid_parent.join("proxies.json"), &Config::default());
    assert!(pool.is_err());
    let mut pool = ProxyPool::load(directory.path(), &Config::default()).unwrap();
    pool.path = invalid_parent.join("proxies.json");
    let error = pool.import(INPUT).unwrap_err().to_string();
    assert!(!error.contains(PASSWORD));
    assert!(!pool.is_configured());
    assert!(pool.summaries().is_empty());
}

#[test]
fn automatic_checks_are_staggered_and_oldest_routes_are_not_starved() {
    let (pool, _directory) = pool();
    let input = (1..=40)
        .map(|id| format!("gateway.example:{}:demo-user:{PASSWORD}", 10000 + id))
        .collect::<Vec<_>>()
        .join("\n");
    pool.import(&input).unwrap();
    let first = pool.due_ids(Utc::now());
    assert_eq!(first, (1..=25).collect::<Vec<_>>());
    for entry in &mut pool.inner.lock().entries[..25] {
        entry.record.status = ProxyStatus::Unhealthy;
        entry.record.last_checked_at = Some(Utc::now());
        entry.record.cooldown_until = Some(Utc::now() - chrono::Duration::seconds(1));
    }
    let next = pool.due_ids(Utc::now());
    assert_eq!(&next[..15], &(26..=40).collect::<Vec<_>>());
    assert_eq!(next.len(), 25);
}

#[tokio::test]
async fn one_probe_per_proxy_and_concurrent_waiters_share_the_result() {
    let (pool, _directory) = pool();
    pool.import(INPUT).unwrap();
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let first = {
        let pool = pool.clone();
        let started = started.clone();
        let release = release.clone();
        let calls = calls.clone();
        tokio::spawn(async move {
            pool.check_with(1, |_| async move {
                calls.fetch_add(1, Ordering::SeqCst);
                started.notify_one();
                release.notified().await;
                Some("203.0.113.10".to_string())
            })
            .await
        })
    };
    started.notified().await;
    let second = {
        let pool = pool.clone();
        tokio::spawn(async move {
            pool.check_with(1, |_| async {
                panic!("duplicate probe");
                #[allow(unreachable_code)]
                None
            })
            .await
        })
    };
    tokio::task::yield_now().await;
    release.notify_one();
    assert_eq!(first.await.unwrap().unwrap().status, ProxyStatus::Healthy);
    assert_eq!(second.await.unwrap().unwrap().status, ProxyStatus::Healthy);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(!pool.summary(1).unwrap().checking);
}

#[tokio::test]
async fn health_probes_are_bounded_to_four_and_release_slots_on_cancellation() {
    let (pool, _directory) = pool();
    let lines = (1..=12)
        .map(|i| format!("gateway.example:{}:demo-user:{PASSWORD}", 10000 + i))
        .collect::<Vec<_>>()
        .join("\n");
    pool.import(&lines).unwrap();
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let results = futures::future::join_all((1..=12).map(|id| {
        let pool = pool.clone();
        let active = active.clone();
        let peak = peak.clone();
        async move {
            pool.check_with(id, |_| async move {
                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(30)).await;
                active.fetch_sub(1, Ordering::SeqCst);
                Some("203.0.113.10".to_string())
            })
            .await
        }
    }))
    .await;
    assert!(results.into_iter().all(|r| r.is_ok()));
    assert_eq!(peak.load(Ordering::SeqCst), 4);
    let started = Arc::new(Notify::new());
    let pending = {
        let pool = pool.clone();
        let started = started.clone();
        tokio::spawn(async move {
            pool.check_with(1, |_| async move {
                started.notify_one();
                futures::future::pending::<Option<String>>().await
            })
            .await
        })
    };
    started.notified().await;
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    assert!(!pool.summary(1).unwrap().checking);
    assert_eq!(pool.slots.available_permits(), 4);
}

#[tokio::test]
async fn late_probe_cannot_override_disable_reenable_or_deleted_record() {
    let (pool, _directory) = pool();
    pool.import(INPUT).unwrap();
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let task = {
        let pool = pool.clone();
        let started = started.clone();
        let release = release.clone();
        tokio::spawn(async move {
            pool.check_with(1, |_| async move {
                started.notify_one();
                release.notified().await;
                Some("203.0.113.10".to_string())
            })
            .await
        })
    };
    started.notified().await;
    pool.set_enabled(1, false).unwrap();
    pool.set_enabled(1, true).unwrap();
    release.notify_one();
    assert!(task.await.unwrap().is_err());
    assert_eq!(pool.summary(1).unwrap().status, ProxyStatus::Unknown);
    assert!(pool.get_healthy_route(1).is_none());
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let task = {
        let pool = pool.clone();
        let started = started.clone();
        let release = release.clone();
        tokio::spawn(async move {
            pool.check_with(1, |_| async move {
                started.notify_one();
                release.notified().await;
                Some("203.0.113.10".to_string())
            })
            .await
        })
    };
    started.notified().await;
    pool.delete(1).unwrap();
    pool.import(INPUT).unwrap();
    release.notify_one();
    assert!(task.await.unwrap().is_err());
    assert_eq!(pool.summary(2).unwrap().status, ProxyStatus::Unknown);
}

#[tokio::test]
async fn queued_disabled_proxy_never_starts_probe_and_capacity_timeout_is_inconclusive() {
    let directory = Directory::new();
    let mut pool = ProxyPool::load(directory.path(), &Config::default()).unwrap();
    pool.timeout = Duration::from_millis(70);
    pool.import(INPUT).unwrap();
    let pool = Arc::new(pool);
    healthy(&pool);
    let held = pool.slots.clone().acquire_many_owned(4).await.unwrap();
    let result = pool
        .check_with(1, |_| async {
            panic!("must not send without capacity");
            #[allow(unreachable_code)]
            None
        })
        .await;
    assert!(result.is_err());
    assert_eq!(pool.summary(1).unwrap().status, ProxyStatus::Healthy);
    assert!(!pool.route_requires_reassignment(1));
    assert!(pool.confirm_failure(1).await.is_err());
    assert_eq!(pool.summary(1).unwrap().status, ProxyStatus::Healthy);
    assert!(!pool.route_requires_reassignment(1));
    let task = {
        let pool = pool.clone();
        tokio::spawn(async move {
            pool.check_with(1, |_| async {
                panic!("disabled queue must not send");
                #[allow(unreachable_code)]
                None
            })
            .await
        })
    };
    tokio::task::yield_now().await;
    pool.set_enabled(1, false).unwrap();
    drop(held);
    assert!(task.await.unwrap().is_err());
    assert!(!pool.summary(1).unwrap().enabled);
    assert!(!pool.summary(1).unwrap().checking);
}

async fn wait_for_confirmation(pool: &ProxyPool) {
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if pool.inner.lock().entries[0]
                .checking
                .as_ref()
                .is_some_and(|flight| flight.quarantine.load(Ordering::Acquire))
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("confirmation waiter joins the existing flight");
}

async fn wait_for_check(pool: &ProxyPool) {
    tokio::time::timeout(Duration::from_secs(1), async {
        while !pool.summary(1).unwrap().checking {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("owner claims its flight");
}

#[tokio::test]
async fn confirmation_waiter_cannot_reuse_cached_status_after_owner_cancellation() {
    for cached in [ProxyStatus::Healthy, ProxyStatus::Unhealthy] {
        let (pool, _directory) = pool();
        pool.import(INPUT).unwrap();
        pool.inner.lock().entries[0].record.status = cached;
        let held = pool.slots.clone().acquire_many_owned(4).await.unwrap();
        let owner = {
            let pool = pool.clone();
            tokio::spawn(async move {
                pool.check_with(1, |_| async {
                    panic!("capacity queue must not start network");
                    #[allow(unreachable_code)]
                    None
                })
                .await
            })
        };
        wait_for_check(&pool).await;
        let flight = pool.inner.lock().entries[0]
            .checking
            .as_ref()
            .unwrap()
            .clone();
        let waiter = {
            let pool = pool.clone();
            tokio::spawn(async move { pool.confirm_failure(1).await })
        };
        wait_for_confirmation(&pool).await;
        owner.abort();
        assert!(owner.await.unwrap_err().is_cancelled());
        assert!(waiter.await.unwrap().is_err());
        assert!(flight.done.load(Ordering::Acquire));
        assert!(flight.result.lock().is_none());
        assert_eq!(pool.summary(1).unwrap().status, cached);
        assert!(!pool.summary(1).unwrap().checking);
        drop(held);
    }
}

#[tokio::test]
async fn confirmation_waiter_cannot_reuse_cached_status_after_owner_queue_timeout() {
    for cached in [ProxyStatus::Healthy, ProxyStatus::Unhealthy] {
        let directory = Directory::new();
        let mut pool = ProxyPool::load(directory.path(), &Config::default()).unwrap();
        pool.timeout = Duration::from_millis(150);
        pool.import(INPUT).unwrap();
        pool.inner.lock().entries[0].record.status = cached;
        let pool = Arc::new(pool);
        let held = pool.slots.clone().acquire_many_owned(4).await.unwrap();
        let owner = {
            let pool = pool.clone();
            tokio::spawn(async move {
                pool.check_with(1, |_| async {
                    panic!("capacity queue must not start network");
                    #[allow(unreachable_code)]
                    None
                })
                .await
            })
        };
        wait_for_check(&pool).await;
        let waiter = {
            let pool = pool.clone();
            tokio::spawn(async move { pool.confirm_failure(1).await })
        };
        wait_for_confirmation(&pool).await;
        assert!(owner.await.unwrap().is_err());
        assert!(waiter.await.unwrap().is_err());
        assert_eq!(pool.summary(1).unwrap().status, cached);
        assert!(!pool.summary(1).unwrap().checking);
        drop(held);
    }
}

#[tokio::test]
async fn failed_storage_commit_never_publishes_a_confirmation_ticket() {
    for cached in [ProxyStatus::Healthy, ProxyStatus::Unhealthy] {
        let directory = Directory::new();
        let mut pool = ProxyPool::load(directory.path(), &Config::default()).unwrap();
        pool.import(INPUT).unwrap();
        pool.inner.lock().entries[0].record.status = cached;
        let blocker = directory.0.join("not-a-directory");
        std::fs::write(&blocker, "test").unwrap();
        pool.path = blocker.join("proxies.json");
        let pool = Arc::new(pool);
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let owner = {
            let pool = pool.clone();
            let started = started.clone();
            let release = release.clone();
            tokio::spawn(async move {
                pool.check_with(1, |_| async move {
                    started.notify_one();
                    release.notified().await;
                    Some("203.0.113.10".to_string())
                })
                .await
            })
        };
        started.notified().await;
        let flight = pool.inner.lock().entries[0]
            .checking
            .as_ref()
            .unwrap()
            .clone();
        let waiter = {
            let pool = pool.clone();
            tokio::spawn(async move { pool.confirm_failure(1).await })
        };
        wait_for_confirmation(&pool).await;
        release.notify_one();
        assert!(owner.await.unwrap().is_err());
        assert!(waiter.await.unwrap().is_err());
        assert!(flight.result.lock().is_none());
        assert!(flight.done.load(Ordering::Acquire));
    }
}

#[tokio::test]
async fn committed_ticket_is_published_before_done_and_cannot_survive_a_version_change() {
    let (pool, _directory) = pool();
    pool.import(INPUT).unwrap();
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let owner = {
        let pool = pool.clone();
        let started = started.clone();
        let release = release.clone();
        tokio::spawn(async move {
            pool.check_with(1, |_| async move {
                started.notify_one();
                release.notified().await;
                Some("203.0.113.10".to_string())
            })
            .await
        })
    };
    started.notified().await;
    let flight = pool.inner.lock().entries[0]
        .checking
        .as_ref()
        .unwrap()
        .clone();
    assert!(flight.result.lock().is_none());
    assert!(!flight.done.load(Ordering::Acquire));
    release.notify_one();
    let result = owner.await.unwrap().unwrap();
    assert!(flight.done.load(Ordering::Acquire));
    let ticket = flight.result.lock().clone().unwrap();
    assert_eq!(ticket.last_checked_at, result.last_checked_at);
    assert_eq!(ticket.exit_ip.as_deref(), Some("203.0.113.10"));
    assert!(!ticket.checking);
    pool.set_enabled(1, false).unwrap();
    pool.set_enabled(1, true).unwrap();
    assert!(pool.completed_result(1, &flight).is_err());
}

#[tokio::test]
async fn only_completed_failed_probe_marks_unhealthy_and_success_clears_cooldown() {
    let (pool, _directory) = pool();
    pool.import(INPUT).unwrap();
    let failed = pool.check_with(1, |_| async { None }).await.unwrap();
    assert_eq!(failed.status, ProxyStatus::Unhealthy);
    assert_eq!(failed.failure_count, 1);
    assert!(pool.route_requires_reassignment(1));
    assert!(failed.cooldown_until.unwrap() > Utc::now());
    assert_eq!(pool.check_due().await, 0); // 没有网络探测，冷却期尚未到。
    let recovered = pool
        .check_with(1, |_| async { Some("203.0.113.10".to_string()) })
        .await
        .unwrap();
    assert_eq!(recovered.status, ProxyStatus::Healthy);
    assert_eq!(recovered.failure_count, 0);
    assert!(recovered.cooldown_until.is_none());
}

struct Server(tokio::task::JoinHandle<()>);
impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn transport_pool(router: axum::Router) -> (Arc<ProxyPool>, Directory, Server) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let directory = Directory::new();
    let mut pool = ProxyPool::load(directory.path(), &Config::default()).unwrap();
    pool.health_url = "http://health-target.example/".to_string(); // 私有测试覆盖；公开配置始终要求 HTTPS。
    pool.import(&format!("127.0.0.1:{port}:demo-user:{PASSWORD}"))
        .unwrap();
    (Arc::new(pool), directory, Server(task))
}

#[tokio::test]
async fn local_proxy_transport_auth_is_correct_and_non_ip_or_oversized_body_is_rejected() {
    use base64::Engine;
    let expected = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("demo-user:{PASSWORD}"))
    );
    let router =
        axum::Router::new().fallback(axum::routing::get(move |headers: axum::http::HeaderMap| {
            let expected = expected.clone();
            async move {
                assert_eq!(
                    headers.get("proxy-authorization").unwrap(),
                    expected.as_str()
                );
                "203.0.113.10\n"
            }
        }));
    let (pool, _directory, _server) = transport_pool(router).await;
    assert_eq!(
        pool.check(1).await.unwrap().exit_ip.as_deref(),
        Some("203.0.113.10")
    );
    for body in ["not an IP".to_string(), "x".repeat(2049)] {
        let router = axum::Router::new().fallback(axum::routing::get(move || {
            let body = body.clone();
            async move { body }
        }));
        let (pool, _directory, _server) = transport_pool(router).await;
        assert!(!pool.confirm_failure(1).await.unwrap());
        assert_eq!(pool.summary(1).unwrap().status, ProxyStatus::Unhealthy);
    }
}

#[tokio::test]
async fn healthy_confirmation_preserves_binding_during_probe_and_queue_timeout_cannot_switch() {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let router = axum::Router::new().fallback(axum::routing::get({
        let started = started.clone();
        let release = release.clone();
        move || {
            let started = started.clone();
            let release = release.clone();
            async move {
                started.notify_one();
                release.notified().await;
                "203.0.113.10"
            }
        }
    }));
    let (pool, _directory, _server) = transport_pool(router).await;
    healthy(&pool);
    let confirmation = {
        let pool = pool.clone();
        tokio::spawn(async move { pool.confirm_failure(1).await })
    };
    started.notified().await;
    assert_eq!(pool.summary(1).unwrap().status, ProxyStatus::Healthy);
    assert!(pool.get_healthy_route(1).is_none()); // 暂停可疑路由的新请求，不重新绑定。
    assert!(!pool.route_requires_reassignment(1));
    release.notify_one();
    assert!(confirmation.await.unwrap().unwrap());
    assert!(!pool.route_requires_reassignment(1));
}
