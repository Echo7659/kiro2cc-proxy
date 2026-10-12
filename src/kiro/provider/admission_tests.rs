//! Loopback-only contract tests; synthetic accounts never contact Kiro.
use super::KiroProvider;
use super::admission::{AccountUnavailableError, RateLimitError, retry_after};
use crate::kiro::endpoint::{BUCKET_THROTTLE_DURATION, EndpointName};
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::token_manager::MultiTokenManager;
use crate::model::config::Config;
use axum::{Router, body::Body, routing::post};
use bytes::Bytes;
use futures::{StreamExt, stream};
use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::sync::{Mutex, mpsc};

fn local_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap()
}

fn protection_config(rpm: u32, global: usize, account: usize, timeout: u64) -> Config {
    let mut config = Config::default();
    config.max_rpm_per_credential = rpm;
    config.max_concurrent_requests = global;
    config.max_concurrent_per_credential = account;
    config.upstream_admission_timeout_secs = timeout;
    config
}

fn provider(mut config: Config, accounts: usize) -> KiroProvider {
    config.proxy_url = None;
    let credentials = (1..=accounts)
        .map(|id| KiroCredentials {
            id: Some(id as u64),
            access_token: Some(format!("test-account-{id}")),
            refresh_token: Some(format!("test-{id}-{}", "r".repeat(120))),
            expires_at: Some((chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339()),
            auth_method: Some("social".into()),
            machine_id: Some(format!("{:064x}", id)),
            ..Default::default()
        })
        .collect();
    let manager = MultiTokenManager::new(config, credentials, None, None, true).unwrap();
    let provider = KiroProvider::new(Arc::new(manager));
    provider
        .client_cache
        .lock()
        .insert((None, false), local_client());
    provider
        .client_cache
        .lock()
        .insert((None, true), local_client());
    provider
}

async fn server(router: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (url, task)
}

impl KiroProvider {
    async fn send_test_request(
        &self,
        id: u64,
        request: reqwest::RequestBuilder,
    ) -> anyhow::Result<super::UpstreamResponse> {
        let context = self
            .token_manager
            .acquire_context_sticky(None, &[id], None, &[])
            .await?;
        self.send_admitted(&context, request).await
    }
}

type BodySender = mpsc::Sender<Result<Bytes, std::io::Error>>;

async fn delayed_body() -> (String, BodySender, tokio::task::JoinHandle<()>) {
    let (sender, receiver) = mpsc::channel::<Result<Bytes, std::io::Error>>(4);
    sender.send(Ok(Bytes::from_static(b"first"))).await.unwrap();
    let receiver = Arc::new(Mutex::new(Some(receiver)));
    let router = Router::new().route(
        "/",
        post(move || {
            let receiver = receiver.clone();
            async move {
                let rx = receiver.lock().await.take().unwrap();
                Body::from_stream(stream::unfold(rx, |mut rx| async move {
                    rx.recv().await.map(|item| (item, rx))
                }))
            }
        }),
    );
    let (url, task) = server(router).await;
    (url, sender, task)
}

fn assert_slots(provider: &KiroProvider, global: usize, account: usize) {
    assert_eq!(provider.concurrency_limit.available_permits(), global);
    assert_eq!(provider.semaphore_for(1).available_permits(), account);
}

#[tokio::test]
async fn stream_body_keeps_both_leases_after_headers_and_releases_on_eof() {
    let (url, sender, server) = delayed_body().await;
    let provider = provider(Config::default(), 1);
    let response = provider
        .send_test_request(1, local_client().post(url))
        .await
        .unwrap();
    assert_slots(&provider, 19, 1);
    assert_eq!(
        provider.token_manager.snapshot().entries[0].success_count,
        0
    );
    let mut body = response.bytes_stream();
    assert_eq!(body.next().await.unwrap().unwrap(), "first");
    assert_slots(&provider, 19, 1);
    drop(sender);
    assert!(body.next().await.is_none());
    assert_slots(&provider, 20, 2);
    assert_eq!(
        provider.token_manager.snapshot().entries[0].success_count,
        1
    );
    // EOF releases immediately even if the completed stream itself remains alive.
    assert!(body.next().await.is_none());
    server.abort();
}

#[tokio::test]
async fn dropping_response_or_stream_releases_without_marking_success() {
    for consume_first in [false, true] {
        let (url, _sender, server) = delayed_body().await;
        let provider = provider(Config::default(), 1);
        let response = provider
            .send_test_request(1, local_client().post(url))
            .await
            .unwrap();
        if consume_first {
            let mut body = response.bytes_stream();
            body.next().await.unwrap().unwrap();
            drop(body);
        } else {
            drop(response);
        }
        assert_slots(&provider, 20, 2);
        assert_eq!(
            provider.token_manager.snapshot().entries[0].success_count,
            0
        );
        server.abort();
    }
}

#[tokio::test]
async fn body_transport_error_releases_without_marking_success() {
    let (url, sender, server) = delayed_body().await;
    let provider = provider(Config::default(), 1);
    let response = provider
        .send_test_request(1, local_client().post(url))
        .await
        .unwrap();
    let mut body = response.bytes_stream();
    body.next().await.unwrap().unwrap();
    sender
        .send(Err(std::io::Error::other("synthetic body disconnect")))
        .await
        .unwrap();
    assert!(body.next().await.unwrap().is_err());
    assert_slots(&provider, 20, 2);
    assert_eq!(
        provider.token_manager.snapshot().entries[0].success_count,
        0
    );
    server.abort();
}

#[tokio::test]
async fn cancelling_nonstream_body_read_releases_both_leases() {
    let (url, _sender, server) = delayed_body().await;
    let provider = provider(Config::default(), 1);
    let response = provider
        .send_test_request(1, local_client().post(url))
        .await
        .unwrap();
    let task = tokio::spawn(response.text());
    tokio::task::yield_now().await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_slots(&provider, 20, 2);
    assert_eq!(
        provider.token_manager.snapshot().entries[0].success_count,
        0
    );
    server.abort();
}

#[tokio::test]
async fn api_mcp_and_nonstream_responses_hold_lease_until_body_completion() {
    for mode in 0..3 {
        let (url, sender, server) = delayed_body().await;
        let mut provider = provider(Config::default(), 1);
        provider.upstream_url_override = Some(url);
        let (response, id) = match mode {
            0 => provider.call_api_stream("{}", false, false, &[]).await,
            1 => provider.call_api("{}", false, false, &[]).await,
            _ => provider.call_mcp("{}", &[]).await,
        }
        .unwrap();
        assert_eq!(id, 1);
        assert_slots(&provider, 19, 1);
        drop(sender);
        assert_eq!(response.bytes().await.unwrap(), "first");
        assert_slots(&provider, 20, 2);
        assert_eq!(provider.rpm_tracker.as_ref().unwrap().credential_rpm(1), 1);
        server.abort();
    }
}

#[tokio::test]
async fn full_rpm_in_single_account_pool_rejects_without_a_second_network_send() {
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = hits.clone();
    let (url, server) = server(Router::new().route(
        "/",
        post(move || {
            let seen = seen.clone();
            async move {
                seen.fetch_add(1, Ordering::SeqCst);
                "ok"
            }
        }),
    ))
    .await;
    let mut provider = provider(protection_config(1, 20, 2, 1), 1);
    provider.upstream_url_override = Some(url);
    provider
        .call_api("{}", false, false, &[])
        .await
        .unwrap()
        .0
        .bytes()
        .await
        .unwrap();
    let error = provider
        .call_api_stream("{}", false, false, &[])
        .await
        .unwrap_err();
    let rate = error.downcast_ref::<RateLimitError>().unwrap();
    assert!(rate.retry_after_secs() >= 59);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert_slots(&provider, 20, 2);
    server.abort();
}

#[tokio::test]
async fn parallel_actual_sends_are_atomically_admitted_and_failed_attempts_count() {
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = hits.clone();
    let (url, server) = server(Router::new().route(
        "/",
        post(move || {
            let seen = seen.clone();
            async move {
                seen.fetch_add(1, Ordering::SeqCst);
                (http::StatusCode::SERVICE_UNAVAILABLE, "failed")
            }
        }),
    ))
    .await;
    let provider = Arc::new(provider(protection_config(10, 20, 2, 1), 1));
    let results = futures::future::join_all((0..30).map(|_| {
        let provider = provider.clone();
        let url = url.clone();
        async move {
            match provider
                .send_test_request(1, local_client().post(url))
                .await
            {
                Ok(response) => {
                    response.text().await.unwrap();
                    true
                }
                Err(error) => {
                    assert!(error.downcast_ref::<RateLimitError>().is_some());
                    false
                }
            }
        }
    }))
    .await;
    assert_eq!(results.into_iter().filter(|ok| *ok).count(), 10);
    assert_eq!(hits.load(Ordering::SeqCst), 10);
    assert_eq!(provider.rpm_tracker.as_ref().unwrap().credential_rpm(1), 10);
    assert_eq!(
        provider.token_manager.snapshot().entries[0].success_count,
        0
    );
    assert_slots(&provider, 20, 2);
    server.abort();
}

#[tokio::test]
async fn rpm_or_busy_account_waiters_do_not_starve_other_accounts() {
    let (url, server) = server(Router::new().route("/", post(|| async { "ok" }))).await;
    let provider = Arc::new(provider(protection_config(10, 1, 1, 1), 2));
    let held = provider.semaphore_for(1).acquire_owned().await.unwrap();
    let pending = {
        let provider = provider.clone();
        let url = url.clone();
        tokio::spawn(async move {
            provider
                .send_test_request(1, local_client().post(url))
                .await
        })
    };
    tokio::task::yield_now().await;
    let response = tokio::time::timeout(
        Duration::from_millis(500),
        provider.send_test_request(2, local_client().post(url)),
    )
    .await
    .unwrap()
    .unwrap();
    response.text().await.unwrap();
    let error = pending.await.unwrap().unwrap_err();
    assert!(error.downcast_ref::<RateLimitError>().is_some());
    assert_eq!(provider.rpm_tracker.as_ref().unwrap().credential_rpm(1), 0);
    drop(held);
    server.abort();
}

#[tokio::test]
async fn active_body_enforces_global_and_account_limit_until_cancelled() {
    let (url, _sender, server) = delayed_body().await;
    let provider = Arc::new(provider(protection_config(10, 1, 1, 1), 2));
    let response = provider
        .send_test_request(1, local_client().post(&url))
        .await
        .unwrap();
    let mut other = Box::pin(provider.send_test_request(2, local_client().post(&url)));
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut other)
            .await
            .is_err()
    );
    assert_eq!(provider.rpm_tracker.as_ref().unwrap().credential_rpm(2), 0);
    drop(other);
    drop(response);
    assert_eq!(provider.concurrency_limit.available_permits(), 1);
    assert_eq!(provider.semaphore_for(1).available_permits(), 1);
    assert_eq!(provider.semaphore_for(2).available_permits(), 1);
    server.abort();
}

#[tokio::test]
async fn queued_request_rechecks_disabled_account_before_rpm_reservation_or_send() {
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = hits.clone();
    let (url, server) = server(Router::new().route(
        "/",
        post(move || {
            let seen = seen.clone();
            async move {
                seen.fetch_add(1, Ordering::SeqCst);
                "ok"
            }
        }),
    ))
    .await;
    let provider = provider(protection_config(10, 1, 1, 1), 1);
    let held = provider.semaphore_for(1).acquire_owned().await.unwrap();
    let mut queued = Box::pin(provider.send_test_request(1, local_client().post(url)));
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut queued)
            .await
            .is_err()
    );
    provider.token_manager.set_disabled(1, true).await.unwrap();
    drop(held);
    let error = queued.await.unwrap_err();
    assert!(error.downcast_ref::<AccountUnavailableError>().is_some());
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    assert_eq!(provider.rpm_tracker.as_ref().unwrap().credential_rpm(1), 0);
    assert_slots(&provider, 1, 1);
    server.abort();
}

#[tokio::test]
async fn queued_old_context_rebuilds_fresh_token_identity_before_api_and_mcp_send() {
    for mcp in [false, true] {
        for identity_change in [false, true] {
            let (sender, receiver) = mpsc::channel::<Result<Bytes, std::io::Error>>(2);
            sender
                .send(Ok(Bytes::from_static(b"holding")))
                .await
                .unwrap();
            let receiver = Arc::new(Mutex::new(Some(receiver)));
            let calls = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
            let seen = calls.clone();
            let router = Router::new().route(
                "/",
                post(move |headers: http::HeaderMap| {
                    let receiver = receiver.clone();
                    let seen = seen.clone();
                    async move {
                        seen.lock().await.push((
                            headers[http::header::AUTHORIZATION]
                                .to_str()
                                .unwrap()
                                .to_owned(),
                            headers[http::header::USER_AGENT]
                                .to_str()
                                .unwrap()
                                .to_owned(),
                        ));
                        if let Some(rx) = receiver.lock().await.take() {
                            Body::from_stream(stream::unfold(rx, |mut rx| async move {
                                rx.recv().await.map(|item| (item, rx))
                            }))
                        } else {
                            Body::from("fresh")
                        }
                    }
                }),
            );
            let (url, server) = server(router).await;
            let mut provider = provider(protection_config(10, 1, 1, 1), 1);
            provider.upstream_url_override = Some(url);
            let first = provider
                .call_api_stream("{}", false, false, &[])
                .await
                .unwrap()
                .0;
            let mut queued = Box::pin(async {
                if mcp {
                    provider.call_mcp("{}", &[]).await
                } else {
                    provider.call_api_stream("{}", false, false, &[]).await
                }
            });
            assert!(
                tokio::time::timeout(Duration::from_millis(50), &mut queued)
                    .await
                    .is_err()
            );
            {
                let mut entries = provider.token_manager.entries.lock();
                entries[0].credentials.access_token = Some("replacement-token".into());
                if identity_change {
                    entries[0].credentials.refresh_token = Some("new-identity-refresh".repeat(10));
                    entries[0].credentials.machine_id = Some("b".repeat(64));
                    entries[0].credentials.profile_arn = Some("arn:test:replacement".into());
                }
            }
            drop(first);
            let response = queued.await.unwrap().0;
            assert_eq!(response.text().await.unwrap(), "fresh");
            let calls = calls.lock().await;
            assert_eq!(calls.len(), 2);
            assert_eq!(calls[0].0, "Bearer test-account-1");
            assert_eq!(calls[1].0, "Bearer replacement-token");
            if identity_change {
                assert!(calls[1].1.contains(&"b".repeat(64)));
            }
            assert_eq!(provider.rpm_tracker.as_ref().unwrap().credential_rpm(1), 2);
            assert_eq!(
                provider.token_manager.snapshot().entries[0].failure_count,
                0
            );
            drop(sender);
            server.abort();
        }
    }
}

#[tokio::test]
async fn old_inflight_error_response_cannot_increment_new_identity_failures() {
    let (sender, receiver) = mpsc::channel::<Result<Bytes, std::io::Error>>(2);
    sender
        .send(Ok(Bytes::from_static(b"old authentication error")))
        .await
        .unwrap();
    let receiver = Arc::new(Mutex::new(Some(receiver)));
    let (started_tx, mut started_rx) = mpsc::channel(1);
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let router = Router::new().route(
        "/",
        post(move || {
            let receiver = receiver.clone();
            let started_tx = started_tx.clone();
            let seen = seen.clone();
            async move {
                seen.fetch_add(1, Ordering::SeqCst);
                if let Some(rx) = receiver.lock().await.take() {
                    started_tx.send(()).await.unwrap();
                    (
                        http::StatusCode::UNAUTHORIZED,
                        Body::from_stream(stream::unfold(rx, |mut rx| async move {
                            rx.recv().await.map(|item| (item, rx))
                        })),
                    )
                } else {
                    (http::StatusCode::OK, Body::from("fresh"))
                }
            }
        }),
    );
    let (url, server) = server(router).await;
    let mut provider = provider(protection_config(10, 1, 1, 1), 1);
    provider.upstream_url_override = Some(url);
    let mut request = Box::pin(provider.call_api("{}", false, false, &[]));
    tokio::select! { _ = started_rx.recv() => {}, result = &mut request => panic!("response completed prematurely: {result:?}") }
    {
        let mut entries = provider.token_manager.entries.lock();
        entries[0].credentials.access_token = Some("replacement-token".into());
        entries[0].credentials.refresh_token = Some("replacement-refresh".repeat(10));
    }
    drop(sender);
    request.await.unwrap().0.text().await.unwrap();
    let state = provider.token_manager.snapshot();
    assert_eq!(state.entries[0].failure_count, 0);
    assert!(!state.entries[0].disabled);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(provider.rpm_tracker.as_ref().unwrap().credential_rpm(1), 2);
    server.abort();
}

#[tokio::test]
async fn old_success_body_eof_does_not_reset_new_identity_failure_counter() {
    let (url, sender, server) = delayed_body().await;
    let provider = provider(Config::default(), 1);
    let response = provider
        .send_test_request(1, local_client().post(url))
        .await
        .unwrap();
    {
        let mut entries = provider.token_manager.entries.lock();
        entries[0].credentials.access_token = Some("replacement-token".into());
        entries[0].credentials.refresh_token = Some("replacement-refresh".repeat(10));
        entries[0].failure_count = 2;
    }
    drop(sender);
    response.bytes().await.unwrap();
    let state = provider.token_manager.snapshot();
    assert_eq!(state.entries[0].failure_count, 2);
    assert_eq!(state.entries[0].success_count, 0);
    assert_slots(&provider, 20, 2);
    server.abort();
}

#[test]
fn parses_retry_after_seconds_http_date_and_invalid_fallback() {
    let mut headers = http::HeaderMap::new();
    headers.insert(http::header::RETRY_AFTER, "120".parse().unwrap());
    assert_eq!(retry_after(&headers), Duration::from_secs(120));
    let future = (chrono::Utc::now() + chrono::Duration::seconds(120))
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string();
    headers.insert(http::header::RETRY_AFTER, future.parse().unwrap());
    assert!((119..=120).contains(&retry_after(&headers).as_secs()));
    headers.insert(
        http::header::RETRY_AFTER,
        "Thu, 01 Jan 1970 00:00:00 GMT".parse().unwrap(),
    );
    assert_eq!(retry_after(&headers), Duration::from_secs(1));
    headers.insert(http::header::RETRY_AFTER, "invalid".parse().unwrap());
    assert_eq!(retry_after(&headers), BUCKET_THROTTLE_DURATION);
}

#[tokio::test]
async fn upstream_retry_after_blocks_all_endpoints_and_mcp_on_later_requests() {
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = hits.clone();
    let (url, server) = server(Router::new().route(
        "/",
        post(move || {
            let seen = seen.clone();
            async move {
                seen.fetch_add(1, Ordering::SeqCst);
                (
                    http::StatusCode::TOO_MANY_REQUESTS,
                    [("Retry-After", "120")],
                    "limited",
                )
            }
        }),
    ))
    .await;
    let mut provider = provider(Config::default(), 1);
    provider.upstream_url_override = Some(url);
    let error = provider
        .call_api("{}", false, false, &[])
        .await
        .unwrap_err();
    assert!(
        error
            .downcast_ref::<RateLimitError>()
            .unwrap()
            .retry_after_secs()
            >= 119
    );
    for name in EndpointName::ALL {
        assert!(provider.endpoint_registry.is_throttled(1, name));
    }
    let error = provider.call_mcp("{}", &[]).await.unwrap_err();
    assert!(error.downcast_ref::<RateLimitError>().is_some());
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn all_closed_buckets_return_typed_rate_limit_without_network_or_spin() {
    let provider = provider(Config::default(), 1);
    for name in EndpointName::ALL {
        provider
            .endpoint_registry
            .throttle(1, name, Duration::from_secs(30));
    }
    let result = tokio::time::timeout(
        Duration::from_millis(100),
        provider.call_api("{}", false, false, &[]),
    )
    .await
    .unwrap();
    assert!(
        result
            .unwrap_err()
            .downcast_ref::<RateLimitError>()
            .is_some()
    );
    assert_eq!(provider.rpm_tracker.as_ref().unwrap().credential_rpm(1), 0);
}

#[tokio::test]
async fn retry_send_budget_is_three_per_account_and_nine_total_for_api_and_mcp() {
    for mcp in [false, true] {
        let calls = Arc::new(Mutex::new(HashMap::<String, usize>::new()));
        let seen = calls.clone();
        let (url, server) = server(Router::new().route(
            "/",
            post(move |headers: http::HeaderMap| {
                let seen = seen.clone();
                async move {
                    *seen
                        .lock()
                        .await
                        .entry(
                            headers
                                .get(http::header::AUTHORIZATION)
                                .unwrap()
                                .to_str()
                                .unwrap()
                                .to_owned(),
                        )
                        .or_default() += 1;
                    (http::StatusCode::SERVICE_UNAVAILABLE, "transient")
                }
            }),
        ))
        .await;
        let mut provider = provider(protection_config(0, 20, 2, 30), 4);
        provider.upstream_url_override = Some(url);
        let result = if mcp {
            provider.call_mcp("{}", &[]).await
        } else {
            provider.call_api("{}", false, false, &[]).await
        };
        assert!(result.is_err());
        let calls = calls.lock().await;
        assert_eq!(calls.values().sum::<usize>(), 9);
        assert!(calls.values().all(|count| *count <= 3));
        let rpm = provider.rpm_tracker.as_ref().unwrap().snapshot();
        assert_eq!(rpm.by_credential.values().sum::<u64>(), 9);
        assert!(
            provider
                .token_manager
                .snapshot()
                .entries
                .iter()
                .all(|e| e.failure_count == 0)
        );
        server.abort();
    }
}

#[tokio::test]
async fn bound_pool_never_sends_to_an_unbound_account_during_failover() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let seen = calls.clone();
    let (url, server) = server(Router::new().route(
        "/",
        post(move |headers: http::HeaderMap| {
            let seen = seen.clone();
            async move {
                seen.lock().await.push(
                    headers
                        .get(http::header::AUTHORIZATION)
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .to_owned(),
                );
                (
                    http::StatusCode::TOO_MANY_REQUESTS,
                    [("Retry-After", "120")],
                    "limited",
                )
            }
        }),
    ))
    .await;
    let mut provider = provider(Config::default(), 2);
    provider.upstream_url_override = Some(url);
    let error = provider
        .call_api("{}", false, false, &[2])
        .await
        .unwrap_err();
    assert!(error.downcast_ref::<RateLimitError>().is_some());
    assert_eq!(*calls.lock().await, vec!["Bearer test-account-2"]);
    server.abort();
}
