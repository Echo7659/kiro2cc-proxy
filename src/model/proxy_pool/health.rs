use super::*;
use futures::{StreamExt, stream};
use std::future::Future;
use std::net::IpAddr;
use tokio::time::{Instant, timeout_at};

const MAX_HEALTH_BODY: usize = 2048;
const MAX_AUTO_CHECKS: usize = 25;

struct CheckGuard {
    inner: Arc<Mutex<PoolState>>,
    id: u64,
    flight: Arc<CheckFlight>,
}

impl Drop for CheckGuard {
    fn drop(&mut self) {
        if let Some(entry) = self
            .inner
            .lock()
            .entries
            .iter_mut()
            .find(|e| e.record.id == self.id)
            && entry
                .checking
                .as_ref()
                .is_some_and(|f| Arc::ptr_eq(f, &self.flight))
        {
            entry.checking = None;
        }
        self.flight.done.store(true, Ordering::Release);
        self.flight.notify.notify_waiters();
    }
}

impl ProxyPool {
    pub async fn check(&self, id: u64) -> anyhow::Result<ProxySummary> {
        self.check_with(id, |route| self.probe(route))
            .await
            .map_err(|_| ProxyUnavailableError.into())
    }

    /// 一个账号的传输错误先触发只读探测；公网 IP 检查成功时保留原绑定。
    pub async fn confirm_failure(&self, id: u64) -> anyhow::Result<bool> {
        let summary = self
            .check_with_mode(id, |route| self.probe(route), true)
            .await
            .map_err(|_| anyhow::Error::new(ProxyUnavailableError))?;
        anyhow::ensure!(
            summary.enabled && !summary.checking && summary.status != ProxyStatus::Unknown,
            ProxyUnavailableError
        );
        Ok(summary.status == ProxyStatus::Healthy)
    }

    pub async fn check_all(&self) -> usize {
        let ids = self
            .inner
            .lock()
            .entries
            .iter()
            .filter(|e| e.record.enabled)
            .map(|e| e.record.id)
            .collect();
        self.check_ids(ids).await
    }

    pub async fn check_due(&self) -> usize {
        self.check_ids(self.due_ids(Utc::now())).await
    }

    pub(super) fn due_ids(&self, now: DateTime<Utc>) -> Vec<u64> {
        let interval =
            chrono::Duration::from_std(self.interval).unwrap_or(chrono::Duration::days(7));
        {
            let state = self.inner.lock();
            let mut due: Vec<_> = state
                .entries
                .iter()
                .filter(|e| {
                    e.record.enabled
                        && e.checking.is_none()
                        && e.record.cooldown_until.is_none_or(|until| until <= now)
                        && (e.record.status != ProxyStatus::Healthy
                            || e.record.last_checked_at.is_none_or(|checked| {
                                now.signed_duration_since(checked) >= interval
                            }))
                })
                .collect();
            // 新路由和最久未检查的路由优先，前排故障不能饿死大池中的后排路由。
            due.sort_by_key(|e| (e.record.last_checked_at, e.record.id));
            due.into_iter()
                .take(MAX_AUTO_CHECKS)
                .map(|e| e.record.id)
                .collect()
        }
    }

    async fn check_ids(&self, ids: Vec<u64>) -> usize {
        stream::iter(ids)
            .map(|id| async move { usize::from(self.check(id).await.is_ok()) })
            .buffer_unordered(self.concurrency)
            .fold(0, |total, done| async move { total + done })
            .await
    }

    async fn probe(&self, route: ProxyRoute) -> Option<String> {
        let mut proxy = reqwest::Proxy::all(&route.url).ok()?;
        if let (Some(username), Some(password)) = (&route.username, &route.password) {
            proxy = proxy.basic_auth(username, password);
        }
        // 只允许显式代理，禁止自动系统代理和重定向产生直连/认证泄漏。
        let mut builder = reqwest::Client::builder()
            .no_proxy()
            .proxy(proxy)
            .timeout(self.timeout)
            .redirect(reqwest::redirect::Policy::none());
        builder = match self.tls_backend {
            TlsBackend::Rustls => builder.use_rustls_tls(),
            TlsBackend::NativeTls => builder,
        };
        let response = builder
            .build()
            .ok()?
            .get(&self.health_url)
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let mut body = Vec::new();
        let mut chunks = response.bytes_stream();
        while let Some(chunk) = chunks.next().await {
            let chunk = chunk.ok()?;
            if body.len().saturating_add(chunk.len()) > MAX_HEALTH_BODY {
                return None;
            }
            body.extend_from_slice(&chunk);
        }
        let value = std::str::from_utf8(&body).ok()?.trim();
        value.parse::<IpAddr>().ok().map(|ip| ip.to_string())
    }

    pub(super) async fn check_with<P, F>(&self, id: u64, probe: P) -> anyhow::Result<ProxySummary>
    where
        P: FnOnce(ProxyRoute) -> F,
        F: Future<Output = Option<String>>,
    {
        self.check_with_mode(id, probe, false).await
    }

    async fn check_with_mode<P, F>(
        &self,
        id: u64,
        probe: P,
        quarantine: bool,
    ) -> anyhow::Result<ProxySummary>
    where
        P: FnOnce(ProxyRoute) -> F,
        F: Future<Output = Option<String>>,
    {
        let deadline = Instant::now() + self.timeout;
        let (flight, route, owned) = {
            let mut state = self.inner.lock();
            let entry = state
                .entries
                .iter_mut()
                .find(|e| e.record.id == id && e.record.enabled)
                .ok_or(ProxyUnavailableError)?;
            if let Some(flight) = &entry.checking {
                if quarantine {
                    flight.quarantine.store(true, Ordering::Release);
                }
                (flight.clone(), entry.route(), false)
            } else {
                let flight = Arc::new(CheckFlight {
                    generation: entry.record.generation,
                    quarantine: AtomicBool::new(quarantine),
                    result: Mutex::new(None),
                    done: AtomicBool::new(false),
                    notify: Notify::new(),
                });
                entry.checking = Some(flight.clone());
                (flight, entry.route(), true)
            }
        };
        let mut guard = owned.then(|| CheckGuard {
            inner: self.inner.clone(),
            id,
            flight: flight.clone(),
        });
        if !owned {
            loop {
                let notified = flight.notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if flight.done.load(Ordering::Acquire) {
                    break;
                }
                timeout_at(deadline, notified)
                    .await
                    .map_err(|_| anyhow::anyhow!("Proxy check is still running."))?;
            }
            return self.completed_result(id, &flight);
        }
        let permit = timeout_at(deadline, self.slots.clone().acquire_owned())
            .await
            .map_err(|_| anyhow::anyhow!("Proxy check capacity is temporarily unavailable."))??;
        // 排队期间被禁用/删除的路由不能再创建网络请求。
        if self
            .route(id)
            .is_none_or(|r| r.generation != flight.generation)
        {
            drop(permit);
            drop(guard.take());
            return Err(ProxyUnavailableError.into());
        }
        let started = Instant::now();
        // 容量等待不能缩短实际探测预算并误判健康代理。
        let result = timeout_at(Instant::now() + self.timeout, probe(route))
            .await
            .ok()
            .flatten();
        let latency = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        drop(permit);
        let inner = self.inner.clone();
        let path = self.path.clone();
        let cooldown = self.failure_cooldown;
        let generation = flight.generation;
        let committed_flight = flight.clone();
        tokio::task::spawn_blocking(move || {
            let mut state = inner.lock();
            let Some(entry) = state.entries.iter_mut().find(|e| {
                e.record.id == id
                    && e.record.enabled
                    && e.record.generation == generation
                    && e.checking
                        .as_ref()
                        .is_some_and(|f| Arc::ptr_eq(f, &committed_flight))
            }) else {
                return Err(anyhow::Error::new(ProxyUnavailableError));
            };
            entry.record.last_checked_at = Some(Utc::now());
            entry.record.latency_ms = Some(latency);
            if let Some(ip) = result {
                entry.record.status = ProxyStatus::Healthy;
                entry.record.exit_ip = Some(ip);
                entry.record.failure_count = 0;
                entry.record.cooldown_until = None;
            } else {
                entry.record.status = ProxyStatus::Unhealthy;
                entry.record.exit_ip = None;
                entry.record.failure_count = entry.record.failure_count.saturating_add(1);
                entry.record.cooldown_until = Utc::now().checked_add_signed(
                    chrono::Duration::from_std(cooldown).unwrap_or(chrono::Duration::hours(1)),
                );
            }
            let mut summary = entry.summary();
            summary.checking = false;
            // 健康写盘失败时仍保留内存中的故障状态，不能因存储错误重新放行坏路由。
            storage::save(&path, &state)?;
            // 在同一临界区发布本轮票据；取消 guard 清理和 done release 必须在此之后。
            *committed_flight.result.lock() = Some(summary);
            Ok(())
        })
        .await
        .map_err(|_| anyhow::anyhow!("Proxy check could not finish."))??;
        drop(guard.take());
        self.completed_result(id, &flight)
    }

    pub(super) fn completed_result(
        &self,
        id: u64,
        flight: &Arc<CheckFlight>,
    ) -> anyhow::Result<ProxySummary> {
        let state = self.inner.lock();
        let result = flight.result.lock().clone().ok_or(ProxyUnavailableError)?;
        let entry = state
            .entries
            .iter()
            .find(|entry| entry.record.id == id)
            .ok_or(ProxyUnavailableError)?;
        anyhow::ensure!(
            entry.record.enabled
                && entry.record.generation == flight.generation
                && entry.record.last_checked_at == result.last_checked_at
                && entry.record.status == result.status
                && entry
                    .checking
                    .as_ref()
                    .is_none_or(|current| Arc::ptr_eq(current, flight)),
            ProxyUnavailableError
        );
        Ok(result)
    }
}
