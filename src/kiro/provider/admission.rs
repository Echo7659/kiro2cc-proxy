//! Upstream admission and body-lifetime leases. Waiting never holds shared capacity.
use std::fmt;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use futures::{Stream, StreamExt};
use reqwest::{Response, StatusCode, header::HeaderMap};
use serde::de::DeserializeOwned;
use tokio::sync::OwnedSemaphorePermit;
use tokio::time::{Instant, sleep, timeout_at};

use super::core::KiroProvider;
use crate::kiro::endpoint::BUCKET_THROTTLE_DURATION;
use crate::kiro::token_manager::{CallContext, MultiTokenManager};

#[derive(Debug)]
pub struct RateLimitError {
    pub retry_after: Duration,
    pub reason: &'static str,
    pub credential_id: Option<u64>,
}

impl RateLimitError {
    pub fn retry_after_secs(&self) -> u64 {
        self.retry_after
            .as_secs()
            .saturating_add(u64::from(self.retry_after.subsec_nanos() > 0))
            .max(1)
    }
}

impl fmt::Display for RateLimitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(id) = self.credential_id {
            write!(
                f,
                "{} (account {}); retry after {} seconds",
                self.reason,
                id,
                self.retry_after_secs()
            )
        } else {
            write!(
                f,
                "{}; retry after {} seconds",
                self.reason,
                self.retry_after_secs()
            )
        }
    }
}
impl std::error::Error for RateLimitError {}

#[derive(Debug)]
pub struct AccountUnavailableError;

impl fmt::Display for AccountUnavailableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Selected upstream account is no longer available")
    }
}
impl std::error::Error for AccountUnavailableError {}

#[derive(Debug)]
pub struct StaleContextError;

impl fmt::Display for StaleContextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Upstream account context changed while queued or in flight")
    }
}
impl std::error::Error for StaleContextError {}

struct ResponseLease {
    _global: OwnedSemaphorePermit,
    _credential: OwnedSemaphorePermit,
    success: Option<(Arc<MultiTokenManager>, CallContext)>,
}

impl ResponseLease {
    fn complete(mut self) {
        if let Some((manager, context)) = self.success.take() {
            manager.report_success_for_context(&context);
        }
    }
}

/// The lease moves with the response and remains held until the body completes or is dropped.
pub struct UpstreamResponse {
    inner: Response,
    lease: Option<ResponseLease>,
}

impl fmt::Debug for UpstreamResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UpstreamResponse")
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

impl From<Response> for UpstreamResponse {
    fn from(inner: Response) -> Self {
        Self { inner, lease: None }
    }
}

impl UpstreamResponse {
    pub fn status(&self) -> StatusCode {
        self.inner.status()
    }
    pub fn headers(&self) -> &HeaderMap {
        self.inner.headers()
    }

    pub async fn text(self) -> Result<String, reqwest::Error> {
        let Self { inner, lease } = self;
        let result = inner.text().await;
        if result.is_ok()
            && let Some(lease) = lease
        {
            lease.complete();
        }
        result
    }

    pub async fn bytes(self) -> Result<Bytes, reqwest::Error> {
        let Self { inner, lease } = self;
        let result = inner.bytes().await;
        if result.is_ok()
            && let Some(lease) = lease
        {
            lease.complete();
        }
        result
    }

    #[allow(dead_code)] // Same consuming interface as reqwest; retained for MCP JSON consumers.
    pub async fn json<T: DeserializeOwned>(self) -> Result<T, reqwest::Error> {
        let Self { inner, lease } = self;
        let result = inner.json().await;
        if result.is_ok()
            && let Some(lease) = lease
        {
            lease.complete();
        }
        result
    }

    pub fn bytes_stream(self) -> ResponseStream {
        ResponseStream {
            inner: Some(self.inner.bytes_stream().boxed()),
            lease: self.lease,
        }
    }
}

type BodyStream = Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>;

pub struct ResponseStream {
    inner: Option<BodyStream>,
    lease: Option<ResponseLease>,
}

impl Stream for ResponseStream {
    type Item = Result<Bytes, reqwest::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let Some(inner) = self.inner.as_mut() else {
            return Poll::Ready(None);
        };
        match inner.as_mut().poll_next(cx) {
            Poll::Ready(None) => {
                self.inner = None;
                if let Some(lease) = self.lease.take() {
                    lease.complete();
                }
                Poll::Ready(None)
            }
            Poll::Ready(Some(Err(err))) => {
                self.inner = None;
                self.lease = None;
                Poll::Ready(Some(Err(err)))
            }
            other => other,
        }
    }
}

pub(crate) fn retry_after(headers: &HeaderMap) -> Duration {
    let Some(value) = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
    else {
        return BUCKET_THROTTLE_DURATION;
    };
    if let Ok(seconds) = value.trim().parse::<u64>() {
        return Duration::from_secs(seconds.max(1));
    }
    // HTTP-date is RFC 7231's IMF-fixdate, not an ISO timestamp.
    if let Ok(date) = chrono::DateTime::parse_from_rfc2822(value) {
        let remaining = date
            .with_timezone(&chrono::Utc)
            .signed_duration_since(chrono::Utc::now());
        return remaining
            .to_std()
            .unwrap_or(Duration::from_secs(1))
            .max(Duration::from_secs(1));
    }
    BUCKET_THROTTLE_DURATION
}

impl KiroProvider {
    fn admission_error(&self, id: u64, delay: Duration, reason: &'static str) -> anyhow::Error {
        RateLimitError {
            retry_after: delay.max(Duration::from_secs(1)),
            reason,
            credential_id: Some(id),
        }
        .into()
    }

    /// Check delay without consuming capacity. Reservation is atomic and happens after leases
    /// are obtained, so parallel waiters cannot all pass a separate count/read gate.
    async fn admit(&self, context: &CallContext) -> anyhow::Result<ResponseLease> {
        let id = context.id;
        let deadline = Instant::now()
            + Duration::from_secs(self.token_manager.config().upstream_admission_timeout_secs);
        let account_sem = self.semaphore_for(id);
        loop {
            if !self.token_manager.credential_is_available(id) {
                return Err(AccountUnavailableError.into());
            }
            if !self.token_manager.context_is_current(context) {
                return Err(StaleContextError.into());
            }
            if Instant::now() >= deadline {
                return Err(self.admission_error(
                    id,
                    Duration::from_secs(1),
                    "Upstream admission timeout",
                ));
            }
            let max_rpm = self.token_manager.max_rpm_per_credential();
            let cooldown = self.endpoint_registry.account_cooldown_remaining(id);
            let rpm_delay = self
                .rpm_tracker
                .as_ref()
                .and_then(|rpm| rpm.time_until_slot(id, max_rpm));
            if let Some(delay) = match (cooldown, rpm_delay) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            } {
                let reason = if cooldown.is_some() {
                    "Upstream account cooldown"
                } else {
                    "Upstream account RPM limit"
                };
                if delay >= deadline.saturating_duration_since(Instant::now()) {
                    return Err(self.admission_error(id, delay, reason));
                }
                sleep(delay).await;
                continue;
            }
            // Account first: a busy account's queue cannot consume all global slots.
            let credential = timeout_at(deadline, account_sem.clone().acquire_owned())
                .await
                .map_err(|_| {
                    self.admission_error(
                        id,
                        Duration::from_secs(1),
                        "Upstream account concurrency queue timeout",
                    )
                })??;
            let global = timeout_at(deadline, self.concurrency_limit.clone().acquire_owned())
                .await
                .map_err(|_| {
                    self.admission_error(
                        id,
                        Duration::from_secs(1),
                        "Upstream global concurrency queue timeout",
                    )
                })??;
            if !self.token_manager.credential_is_available(id) {
                return Err(AccountUnavailableError.into());
            }
            if !self.token_manager.context_is_current(context) {
                return Err(StaleContextError.into());
            }
            // Retry-After may have arrived from another in-flight request while we queued.
            if self
                .endpoint_registry
                .account_cooldown_remaining(id)
                .is_some()
            {
                drop(global);
                drop(credential);
                continue;
            }
            if let Some(rpm) = &self.rpm_tracker
                && rpm
                    .try_admit_credential(id, self.token_manager.max_rpm_per_credential())
                    .is_err()
            {
                drop(global);
                drop(credential);
                continue;
            }
            return Ok(ResponseLease {
                _global: global,
                _credential: credential,
                success: None,
            });
        }
    }

    pub(crate) async fn send_admitted(
        &self,
        context: &CallContext,
        request: reqwest::RequestBuilder,
    ) -> anyhow::Result<UpstreamResponse> {
        let mut lease = self.admit(context).await?;
        let inner = request.send().await?;
        if inner.status().is_success() {
            lease.success = Some((self.token_manager.clone(), context.clone()));
        }
        Ok(UpstreamResponse {
            inner,
            lease: Some(lease),
        })
    }

    pub(crate) fn record_upstream_throttle(&self, id: u64, headers: &HeaderMap) -> RateLimitError {
        let delay = retry_after(headers);
        self.endpoint_registry.throttle_account(id, delay);
        RateLimitError {
            retry_after: delay,
            reason: "Upstream account rate limit",
            credential_id: Some(id),
        }
    }
}
