// Copyright (c) 2026 Harllan He. Licensed under MIT.
//! RPM（Requests Per Minute）实时监控
//!
//! 使用滑动窗口统计最近 60 秒内的请求数量，
//! 支持全局、按账号、按 API Key 三个维度。

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// 滑动窗口大小（秒）
const WINDOW_SECS: usize = 60;

#[derive(Clone, Copy, Default)]
struct Bucket {
    timestamp: u64,
    count: u64,
}

/// 单个维度的请求时间戳队列（基于 Ring Buffer 的 O(1) 实现）
struct TimestampQueue {
    buckets: [Bucket; WINDOW_SECS],
}

impl TimestampQueue {
    fn new() -> Self {
        Self {
            buckets: [Bucket::default(); WINDOW_SECS],
        }
    }

    /// 记录一次请求
    fn record(&mut self, now_secs: u64) {
        let index = (now_secs as usize) % WINDOW_SECS;
        let bucket = &mut self.buckets[index];
        if bucket.timestamp == now_secs {
            bucket.count += 1;
        } else {
            bucket.timestamp = now_secs;
            bucket.count = 1;
        }
    }

    /// 清理过期条目并返回当前窗口内的请求数
    fn count(&self, now_secs: u64) -> u64 {
        self.buckets
            .iter()
            .filter(|b| now_secs.saturating_sub(b.timestamp) < WINDOW_SECS as u64)
            .map(|b| b.count)
            .sum()
    }
}

/// RPM 追踪器
///
/// 线程安全，使用单个 Mutex 保护所有状态。
/// 内存开销恒定：每个队列固定为 `[Bucket; 60]` 大小。
pub struct RpmTracker {
    inner: Mutex<RpmTrackerInner>,
}

struct RpmTrackerInner {
    /// 全局请求队列
    global: TimestampQueue,
    /// 按账号 ID 分组
    by_credential: HashMap<u64, TimestampQueue>,
    /// 按 API Key ID 分组
    by_api_key: HashMap<u32, TimestampQueue>,
    /// Exact monotonic timestamps for atomic upstream admission. UI buckets remain separate.
    admissions: HashMap<u64, VecDeque<Instant>>,
}

/// RPM 快照（用于 API 响应）
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RpmSnapshot {
    /// 全局 RPM
    pub global: u64,
    /// 按账号 ID 分组的 RPM
    pub by_credential: HashMap<u64, u64>,
    /// 按 API Key ID 分组的 RPM
    pub by_api_key: HashMap<u32, u64>,
}

impl RpmTracker {
    /// 创建新的 RPM 追踪器
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(RpmTrackerInner {
                global: TimestampQueue::new(),
                by_credential: HashMap::new(),
                by_api_key: HashMap::new(),
                admissions: HashMap::new(),
            }),
        }
    }

    /// 记录一次请求（在 handler 入口调用）
    ///
    /// 记录全局 RPM 和 per-API-Key RPM
    pub fn record_request(&self, api_key_id: Option<u32>) {
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut inner = self.inner.lock().unwrap();
        inner.global.record(now_secs);
        if let Some(key_id) = api_key_id {
            inner
                .by_api_key
                .entry(key_id)
                .or_insert_with(TimestampQueue::new)
                .record(now_secs);
        }
    }

    /// Atomically reserve one actual upstream send attempt, including failures/retries.
    /// A denied reservation changes no counters; max_rpm=0 explicitly disables admission limits.
    pub fn try_admit_credential(&self, credential_id: u64, max_rpm: u32) -> Result<(), Duration> {
        let now = Instant::now();
        let mut inner = self.inner.lock().unwrap();
        let queue = inner.admissions.entry(credential_id).or_default();
        while queue
            .front()
            .is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(60))
        {
            queue.pop_front();
        }
        if max_rpm > 0 && queue.len() >= max_rpm as usize {
            return Err(
                Duration::from_secs(60).saturating_sub(now.duration_since(*queue.front().unwrap()))
            );
        }
        queue.push_back(now);
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        inner
            .by_credential
            .entry(credential_id)
            .or_insert_with(TimestampQueue::new)
            .record(now_secs);
        Ok(())
    }

    /// 计算指定账号在 RPM 满时，最快多久后会有一个 slot 释放
    ///
    /// 返回 None 表示当前 RPM 未满或无数据
    pub fn time_until_slot(&self, credential_id: u64, max_rpm: u32) -> Option<std::time::Duration> {
        if max_rpm == 0 {
            return None;
        }
        let now = Instant::now();
        let inner = self.inner.lock().unwrap();
        let queue = inner.admissions.get(&credential_id)?;
        let mut active = queue
            .iter()
            .filter(|t| now.duration_since(**t) < Duration::from_secs(60));
        let oldest = active.next()?;
        if 1 + active.count() < max_rpm as usize {
            return None;
        }
        Some(Duration::from_secs(60).saturating_sub(now.duration_since(*oldest)))
    }

    /// 查询指定账号的当前 RPM
    #[cfg(test)]
    pub fn credential_rpm(&self, credential_id: u64) -> u64 {
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let inner = self.inner.lock().unwrap();
        inner
            .by_credential
            .get(&credential_id)
            .map(|q| q.count(now_secs))
            .unwrap_or(0)
    }

    /// 获取当前 RPM 快照
    pub fn snapshot(&self) -> RpmSnapshot {
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut inner = self.inner.lock().unwrap();

        let global = inner.global.count(now_secs);

        // 获取并顺便清理废弃为空的键，防止长时间未用的 map 残留
        inner
            .by_credential
            .retain(|_, queue| queue.count(now_secs) > 0);
        let by_credential: HashMap<u64, u64> = inner
            .by_credential
            .iter()
            .map(|(&id, queue)| (id, queue.count(now_secs)))
            .collect();

        inner
            .by_api_key
            .retain(|_, queue| queue.count(now_secs) > 0);
        let by_api_key: HashMap<u32, u64> = inner
            .by_api_key
            .iter()
            .map(|(&id, queue)| (id, queue.count(now_secs)))
            .collect();

        RpmSnapshot {
            global,
            by_credential,
            by_api_key,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parallel_admission_never_exceeds_account_rpm() {
        let tracker = std::sync::Arc::new(RpmTracker::new());
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(40));
        let threads: Vec<_> = (0..40)
            .map(|_| {
                let tracker = tracker.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    tracker.try_admit_credential(7, 10).is_ok()
                })
            })
            .collect();
        let admitted = threads
            .into_iter()
            .map(|t| usize::from(t.join().unwrap()))
            .sum::<usize>();
        assert_eq!(admitted, 10);
        assert_eq!(tracker.credential_rpm(7), 10);
        assert!(tracker.time_until_slot(7, 10).unwrap() > Duration::from_secs(59));
        assert!(tracker.try_admit_credential(8, 10).is_ok());
    }

    #[test]
    fn monotonic_admission_has_exact_sixty_second_boundary_and_explicit_unlimited() {
        let tracker = RpmTracker::new();
        tracker.inner.lock().unwrap().admissions.insert(
            1,
            VecDeque::from([Instant::now() - Duration::from_secs(59)]),
        );
        assert!(tracker.try_admit_credential(1, 1).is_err());
        tracker.inner.lock().unwrap().admissions.insert(
            1,
            VecDeque::from([Instant::now() - Duration::from_secs(60)]),
        );
        assert!(tracker.try_admit_credential(1, 1).is_ok());
        for _ in 0..100 {
            assert!(tracker.try_admit_credential(1, 0).is_ok());
        }
        assert!(tracker.time_until_slot(1, 0).is_none());
    }

    #[test]
    fn test_rpm_ring_buffer() {
        let mut queue = TimestampQueue::new();
        // 模拟当前时间
        let now_secs = 1000;

        queue.record(now_secs);
        queue.record(now_secs);

        assert_eq!(queue.count(now_secs), 2);
        assert_eq!(queue.count(now_secs + 10), 2); // 10秒后依然在 60 秒窗口内

        // 测试过期的边界
        assert_eq!(queue.count(now_secs + 60), 0); // 正好 60 秒（已过期）
        assert_eq!(queue.count(now_secs + 61), 0);

        // 模拟一分钟后同一槽位被复用
        queue.record(now_secs + 60);
        assert_eq!(queue.count(now_secs + 60), 1);

        // 模拟多个不同秒数的请求
        queue.record(now_secs + 60);
        queue.record(now_secs + 61);
        queue.record(now_secs + 62);

        assert_eq!(queue.count(now_secs + 62), 4);

        // 当 now_secs 推进到 now_secs + 123 时，所有数据均应过期（最后一次记录在 1062）
        assert_eq!(queue.count(now_secs + 123), 0);
    }
}
