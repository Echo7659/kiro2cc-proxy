//! 持久化代理池：认证信息仅存入私有文件，健康检查与账号绑定相互独立。
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, Semaphore};

use super::config::{Config, TlsBackend};

mod health;
mod parser;
mod storage;
#[cfg(test)]
mod tests;

const MAX_IMPORT_BYTES: usize = 256 * 1024;
const MAX_PROXY_COUNT: usize = 5000;

#[derive(Debug)]
pub struct ProxyUnavailableError;

impl fmt::Display for ProxyUnavailableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Managed proxy is unavailable. Check the proxy pool before retrying.")
    }
}
impl std::error::Error for ProxyUnavailableError {}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyStatus {
    #[default]
    Unknown,
    Healthy,
    Unhealthy,
}

/// 内部路由票据；Debug 和公开 DTO 不包含用户名或密码。
#[derive(Clone, PartialEq, Eq)]
pub struct ProxyRoute {
    pub id: u64,
    pub url: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub generation: u64,
}

impl fmt::Debug for ProxyRoute {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProxyRoute")
            .field("id", &self.id)
            .field("url", &self.url)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxySummary {
    pub id: u64,
    pub url: String,
    pub username_masked: Option<String>,
    pub has_auth: bool,
    pub enabled: bool,
    pub status: ProxyStatus,
    pub last_checked_at: Option<DateTime<Utc>>,
    pub latency_ms: Option<u64>,
    pub exit_ip: Option<String>,
    pub failure_count: u32,
    pub cooldown_until: Option<DateTime<Utc>>,
    pub checking: bool,
    pub assigned_accounts: usize,
    pub assigned_account_ids: Vec<u64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportResult {
    pub imported: usize,
    pub duplicates: usize,
    pub rejected: usize,
    pub errors: Vec<ImportLineError>,
}

#[derive(Debug, Serialize)]
pub struct ImportLineError {
    pub line: usize,
    pub code: &'static str,
    pub message: &'static str,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProxyRecord {
    id: u64,
    url: String,
    username: Option<String>,
    password: Option<String>,
    enabled: bool,
    #[serde(default)]
    generation: u64,
    #[serde(default)]
    status: ProxyStatus,
    #[serde(default)]
    last_checked_at: Option<DateTime<Utc>>,
    #[serde(default)]
    latency_ms: Option<u64>,
    #[serde(default)]
    exit_ip: Option<String>,
    #[serde(default)]
    failure_count: u32,
    #[serde(default)]
    cooldown_until: Option<DateTime<Utc>>,
}

struct CheckFlight {
    generation: u64,
    quarantine: AtomicBool,
    result: Mutex<Option<ProxySummary>>,
    done: AtomicBool,
    notify: Notify,
}

#[derive(Clone)]
struct Entry {
    record: ProxyRecord,
    checking: Option<Arc<CheckFlight>>,
}

#[derive(Clone, Default)]
struct PoolState {
    configured: bool,
    next_id: u64,
    entries: Vec<Entry>,
}

pub struct ProxyPool {
    inner: Arc<Mutex<PoolState>>,
    path: PathBuf,
    slots: Arc<Semaphore>,
    concurrency: usize,
    health_url: String,
    timeout: Duration,
    interval: Duration,
    failure_cooldown: Duration,
    tls_backend: TlsBackend,
}

impl ProxyPool {
    #[cfg(test)]
    pub(crate) fn set_loopback_health_url(&mut self, value: &str) -> anyhow::Result<()> {
        let url = reqwest::Url::parse(value)
            .map_err(|_| anyhow::anyhow!("Invalid loopback test target."))?;
        anyhow::ensure!(
            url.scheme() == "http"
                && matches!(
                    url.host_str(),
                    Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
                )
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "Test health target must be unauthenticated HTTP loopback."
        );
        self.health_url = url.to_string();
        Ok(())
    }

    pub fn load(path: impl AsRef<Path>, config: &Config) -> anyhow::Result<Self> {
        let health_url = parser::validate_health_url(&config.proxy_health_check_url)?;
        let state = storage::load(path.as_ref())?;
        let concurrency = config.proxy_health_check_concurrency.clamp(1, 4);
        Ok(Self {
            inner: Arc::new(Mutex::new(state)),
            path: path.as_ref().to_path_buf(),
            slots: Arc::new(Semaphore::new(concurrency)),
            concurrency,
            health_url,
            timeout: Duration::from_secs(config.proxy_health_check_timeout_secs.clamp(1, 10)),
            interval: Duration::from_secs(config.proxy_health_check_interval_secs.max(1)),
            failure_cooldown: Duration::from_secs(config.proxy_failure_cooldown_secs.max(1)),
            tls_backend: config.tls_backend,
        })
    }

    pub fn is_configured(&self) -> bool {
        self.inner.lock().configured
    }

    pub fn summaries(&self) -> Vec<ProxySummary> {
        self.inner
            .lock()
            .entries
            .iter()
            .map(Entry::summary)
            .collect()
    }

    pub fn summary(&self, id: u64) -> Option<ProxySummary> {
        self.inner
            .lock()
            .entries
            .iter()
            .find(|e| e.record.id == id)
            .map(Entry::summary)
    }

    pub fn route(&self, id: u64) -> Option<ProxyRoute> {
        self.inner
            .lock()
            .entries
            .iter()
            .find(|e| e.record.id == id && e.record.enabled)
            .map(Entry::route)
    }

    pub fn get_healthy_route(&self, id: u64) -> Option<ProxyRoute> {
        self.inner
            .lock()
            .entries
            .iter()
            .find(|e| e.record.id == id && e.is_available())
            .map(Entry::route)
    }

    /// 保留现有好绑定；选择时优先未绑定路由，不够时共享绑定最少的健康路由。
    pub fn select_healthy_proxy(
        &self,
        counts: &HashMap<u64, usize>,
        exclude: &[u64],
    ) -> Option<ProxyRoute> {
        self.inner
            .lock()
            .entries
            .iter()
            .filter(|e| e.is_available() && !exclude.contains(&e.record.id))
            .min_by_key(|e| (counts.get(&e.record.id).copied().unwrap_or(0), e.record.id))
            .map(Entry::route)
    }

    pub fn route_requires_reassignment(&self, id: u64) -> bool {
        let state = self.inner.lock();
        let Some(entry) = state.entries.iter().find(|e| e.record.id == id) else {
            return true;
        };
        !entry.record.enabled
            || (entry.checking.is_none() && entry.record.status == ProxyStatus::Unhealthy)
    }

    pub fn import(&self, text: &str) -> anyhow::Result<ImportResult> {
        anyhow::ensure!(
            text.len() <= MAX_IMPORT_BYTES,
            "Proxy import exceeds the size limit."
        );
        let mut result = ImportResult {
            imported: 0,
            duplicates: 0,
            rejected: 0,
            errors: vec![],
        };
        let mut state = self.inner.lock();
        let previous = state.clone();
        for (offset, line) in text.lines().enumerate() {
            let line = line.trim_start();
            if line.trim().is_empty() {
                continue;
            }
            let parsed = match parser::parse(line) {
                Ok(parsed) => parsed,
                Err(code) => {
                    result.rejected += 1;
                    result.errors.push(ImportLineError {
                        line: offset + 1,
                        code,
                        message: "Invalid proxy address or authentication fields.",
                    });
                    continue;
                }
            };
            if state.entries.iter().any(|e| {
                e.record.url == parsed.url
                    && e.record.username == parsed.username
                    && e.record.password == parsed.password
            }) {
                result.duplicates += 1;
                continue;
            }
            if state.entries.len() >= MAX_PROXY_COUNT {
                result.rejected += 1;
                result.errors.push(ImportLineError {
                    line: offset + 1,
                    code: "limit_exceeded",
                    message: "Proxy pool capacity limit reached.",
                });
                continue;
            }
            let Some(next_id) = state.next_id.checked_add(1) else {
                result.rejected += 1;
                result.errors.push(ImportLineError {
                    line: offset + 1,
                    code: "limit_exceeded",
                    message: "Proxy identifier limit reached.",
                });
                continue;
            };
            state.next_id = next_id;
            let id = state.next_id;
            state.entries.push(Entry {
                record: ProxyRecord {
                    id,
                    url: parsed.url,
                    username: parsed.username,
                    password: parsed.password,
                    enabled: true,
                    generation: 0,
                    status: ProxyStatus::Unknown,
                    last_checked_at: None,
                    latency_ms: None,
                    exit_ip: None,
                    failure_count: 0,
                    cooldown_until: None,
                },
                checking: None,
            });
            result.imported += 1;
        }
        if result.imported > 0 {
            state.configured = true;
            if storage::save(&self.path, &state).is_err() {
                *state = previous;
                anyhow::bail!("Cannot persist proxy pool storage.");
            }
        }
        Ok(result)
    }

    pub fn set_enabled(&self, id: u64, enabled: bool) -> anyhow::Result<ProxySummary> {
        let mut state = self.inner.lock();
        let previous = state.clone();
        let entry = state
            .entries
            .iter_mut()
            .find(|e| e.record.id == id)
            .ok_or(ProxyUnavailableError)?;
        let generation = entry
            .record
            .generation
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("Proxy route version limit reached."))?;
        entry.record.enabled = enabled;
        entry.record.generation = generation;
        entry.record.status = ProxyStatus::Unknown;
        entry.record.cooldown_until = None;
        let summary = entry.summary();
        if storage::save(&self.path, &state).is_err() {
            *state = previous;
            anyhow::bail!("Cannot persist proxy pool storage.");
        }
        Ok(summary)
    }

    pub fn delete(&self, id: u64) -> anyhow::Result<()> {
        let mut state = self.inner.lock();
        let previous = state.clone();
        let length = state.entries.len();
        state.entries.retain(|e| e.record.id != id);
        anyhow::ensure!(state.entries.len() < length, ProxyUnavailableError);
        if storage::save(&self.path, &state).is_err() {
            *state = previous;
            anyhow::bail!("Cannot persist proxy pool storage.");
        }
        Ok(())
    }
}

impl Entry {
    fn is_available(&self) -> bool {
        self.record.enabled
            && self.record.status == ProxyStatus::Healthy
            && !self
                .checking
                .as_ref()
                .is_some_and(|f| f.quarantine.load(Ordering::Acquire))
            && self
                .record
                .cooldown_until
                .is_none_or(|until| until <= Utc::now())
    }

    fn route(&self) -> ProxyRoute {
        ProxyRoute {
            id: self.record.id,
            url: self.record.url.clone(),
            username: self.record.username.clone(),
            password: self.record.password.clone(),
            generation: self.record.generation,
        }
    }

    fn summary(&self) -> ProxySummary {
        ProxySummary {
            id: self.record.id,
            url: self.record.url.clone(),
            username_masked: self
                .record
                .username
                .as_deref()
                .and_then(|u| u.chars().next())
                .map(|c| format!("{c}***")),
            has_auth: self.record.username.is_some(),
            enabled: self.record.enabled,
            status: self.record.status,
            last_checked_at: self.record.last_checked_at,
            latency_ms: self.record.latency_ms,
            exit_ip: self.record.exit_ip.clone(),
            failure_count: self.record.failure_count,
            cooldown_until: self.record.cooldown_until,
            checking: self.checking.is_some(),
            assigned_accounts: 0,
            assigned_account_ids: vec![],
        }
    }
}
