// Copyright (c) 2026 Harllan He. Licensed under MIT.
//! history[0] 漂移诊断（仅 DEBUG 日志，不影响请求内容）
//!
//! 同一会话内 history[0] 的缓存 key 变化（`[exp2] history[0] first`）意味着上游前缀缓存
//! 整段失效。本模块记录每个会话上一次的稳定系统块指纹，key 变化时列出相对上一次
//! 新增 / 消失的系统块（序号、长度、稳定指纹），用来识别 CC 追加了哪些块、
//! 该不该归入动态块（见 `prompt::is_dynamic_hook_injection`）。
//!
//! 系统块与动态块可能含凭据或私有上下文，诊断日志仅输出元数据，不输出任何原文。
//! 仅在 DEBUG 级别输出（Admin 设置页的 `debugLogs` 开关）；未启用时整个模块零开销。
//! 输出量很小：只在 key 变化、或分流块数量变化时各写一行。

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Mutex, OnceLock};

use super::prompt::normalize_billing_header;

/// 最多记录的会话数；超出后整体清空（诊断数据，丢失无影响）
const MAX_SESSIONS: usize = 256;
/// 单次日志最多列出的新增 / 消失块数
const MAX_LISTED: usize = 8;
/// 基线与漂移诊断最多跟踪的系统块数，限制指纹留存与多重集匹配开销。
pub(super) const MAX_TRACKED_BLOCKS: usize = 256;

/// 单个系统块的指纹：内容哈希 + 字节长度
pub(super) type Fingerprint = (u64, usize);

static PREV_BLOCKS: OnceLock<Mutex<HashMap<String, Vec<Fingerprint>>>> = OnceLock::new();
/// 每个会话上一次被分流的动态块数量，用于只在数量变化时写日志
static PREV_DIVERTED: OnceLock<Mutex<HashMap<String, usize>>> = OnceLock::new();

/// 一个新增块：在当前系统区中的序号、字节长度、内容指纹
#[derive(Debug, PartialEq, Eq)]
struct AddedBlock {
    index: usize,
    len: usize,
    fingerprint: u64,
}

pub(super) fn fingerprint(block: &str) -> Fingerprint {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    block.hash(&mut h);
    (h.finish(), block.len())
}

/// 与上一次比较：返回当前新增的块，以及上一次有、这次没有的块长度。
/// 同一指纹出现多次时按次数配对（多重集差）。
fn diff_blocks(prev: &[Fingerprint], current: &[String]) -> (Vec<AddedBlock>, Vec<usize>) {
    let mut remaining: Vec<Option<Fingerprint>> = prev.iter().copied().map(Some).collect();
    let mut added = Vec::new();
    for (index, block) in current.iter().enumerate() {
        let fp = fingerprint(block);
        if let Some(slot) = remaining.iter_mut().find(|s| **s == Some(fp)) {
            *slot = None;
        } else {
            added.push(AddedBlock {
                index,
                len: block.len(),
                fingerprint: fp.0,
            });
        }
    }
    let removed = remaining
        .into_iter()
        .flatten()
        .map(|(_, len)| len)
        .collect();
    (added, removed)
}

/// 记录本次请求的稳定系统块；`key_changed` 为 true（history[0] 缓存 key 首次出现）且该会话
/// 之前有记录时，输出新增 / 消失块。
pub(super) fn observe(session_id: &str, stable_blocks: &[&str], key_changed: bool) {
    if stable_blocks.len() > MAX_TRACKED_BLOCKS || !tracing::enabled!(tracing::Level::DEBUG) {
        return;
    }
    // 与 history[0] 的 key 同口径：先规范化 cch，避免计费哈希的逐轮变化被当成漂移
    let current: Vec<String> = stable_blocks
        .iter()
        .map(|b| normalize_billing_header((*b).to_string()))
        .collect();
    let fps: Vec<Fingerprint> = current.iter().map(|b| fingerprint(b)).collect();

    let map = PREV_BLOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = map.lock().unwrap_or_else(|e| e.into_inner());
    if map.len() >= MAX_SESSIONS && !map.contains_key(session_id) {
        map.clear();
    }
    let prev = map.insert(session_id.to_string(), fps);

    if !key_changed {
        return;
    }
    let Some(prev) = prev else {
        return;
    };
    let (added, removed) = diff_blocks(&prev, &current);
    let listed: Vec<String> = added
        .iter()
        .take(MAX_LISTED)
        .map(|b| format!("#{}({}B) hash={:016x}", b.index, b.len, b.fingerprint))
        .collect();
    tracing::debug!(
        session = %session_id,
        blocks = current.len(),
        added = added.len(),
        removed = removed.len(),
        removed_lens = ?removed.iter().take(MAX_LISTED).collect::<Vec<_>>(),
        "[drift] history[0] 缓存 key 变化，相对上一次的系统块差异：新增 [{}]",
        listed.join(" | ")
    );
}

/// 记录本次请求中被分流为动态块（不进 history[0]）的系统块：块数、各块序号、字节长度与指纹。
/// 用来确认分流规则确实命中（例如任务工具提醒）。同一会话内只在分流块数量变化时输出，
/// 避免动态块持续存在时每轮刷屏；无动态块且数量未变化时不输出。
pub(super) fn log_diverted(session_id: &str, dynamic_blocks: &[&str]) {
    if !tracing::enabled!(tracing::Level::DEBUG) {
        return;
    }
    {
        let map = PREV_DIVERTED.get_or_init(|| Mutex::new(HashMap::new()));
        let mut map = map.lock().unwrap_or_else(|e| e.into_inner());
        if map.len() >= MAX_SESSIONS && !map.contains_key(session_id) {
            map.clear();
        }
        let prev = map.insert(session_id.to_string(), dynamic_blocks.len());
        if prev.unwrap_or(0) == dynamic_blocks.len() {
            return;
        }
    }
    if dynamic_blocks.is_empty() {
        return;
    }
    let listed: Vec<String> = dynamic_blocks
        .iter()
        .enumerate()
        .take(MAX_LISTED)
        .map(|(index, b)| format!("#{}({}B) hash={:016x}", index, b.len(), fingerprint(b).0))
        .collect();
    tracing::debug!(
        session = %session_id,
        diverted = dynamic_blocks.len(),
        "[drift] 动态系统块已分流出 history[0]：[{}]",
        listed.join(" | ")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fps(blocks: &[&str]) -> Vec<Fingerprint> {
        blocks.iter().map(|b| fingerprint(b)).collect()
    }

    fn owned(blocks: &[&str]) -> Vec<String> {
        blocks.iter().map(|b| b.to_string()).collect()
    }

    #[test]
    fn unchanged_blocks_produce_no_diff() {
        let (added, removed) = diff_blocks(&fps(&["a", "b"]), &owned(&["a", "b"]));
        assert!(added.is_empty());
        assert!(removed.is_empty());
    }

    #[test]
    fn appended_block_is_reported_with_index_len_and_fingerprint() {
        let (added, removed) = diff_blocks(&fps(&["core"]), &owned(&["core", "新增\n内容"]));
        assert_eq!(
            added,
            vec![AddedBlock {
                index: 1,
                len: "新增\n内容".len(),
                fingerprint: fingerprint("新增\n内容").0,
            }]
        );
        assert!(removed.is_empty());
    }

    #[test]
    fn changed_block_with_same_length_is_added_and_removed() {
        let (added, removed) = diff_blocks(&fps(&["x=1"]), &owned(&["x=2"]));
        assert_eq!(added.len(), 1);
        assert_eq!(removed, vec![3]);
    }

    #[test]
    fn duplicate_blocks_are_matched_by_count() {
        let (added, removed) = diff_blocks(&fps(&["a", "a"]), &owned(&["a", "a", "a"]));
        assert_eq!(added.len(), 1);
        assert_eq!(added[0].index, 2);
        assert!(removed.is_empty());
    }

    #[derive(Clone, Default)]
    struct BufWriter(std::sync::Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for BufWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn observe_logs_added_block_only_when_key_changed_in_known_session() {
        let buf = BufWriter::default();
        let writer = buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let sid = "drift-test-session";
            // 会话首次出现：没有上一次可比，不输出
            observe(sid, &["core prompt"], true);
            // key 未变：只更新快照，不输出
            observe(sid, &["core prompt"], false);
            // key 变化：输出新增块
            observe(sid, &["core prompt", "APPENDED-BY-CC reminder"], true);
        });
        let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert_eq!(out.matches("[drift]").count(), 1, "{out}");
        assert!(
            out.contains(&format!(
                "hash={:016x}",
                fingerprint("APPENDED-BY-CC reminder").0
            )),
            "{out}"
        );
        assert!(!out.contains("APPENDED-BY-CC reminder"), "{out}");
        assert!(out.contains("added=1"), "{out}");
    }

    #[test]
    fn log_diverted_lists_blocks_and_skips_empty() {
        let buf = BufWriter::default();
        let writer = buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            log_diverted("s", &[]);
            log_diverted("s", &["  The task tools haven't been used recently.\nmore"]);
            // 数量未变化：不重复输出
            log_diverted("s", &["  The task tools haven't been used recently.\nmore"]);
        });
        let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert_eq!(out.matches("[drift]").count(), 1, "{out}");
        assert!(out.contains("diverted=1"), "{out}");
        assert!(out.contains("#0("), "{out}");
        assert!(out.contains("hash="), "{out}");
        assert!(!out.contains("The task tools haven't been used"), "{out}");
    }

    #[test]
    fn debug_diagnostics_never_log_synthetic_private_prompt_text() {
        let stable_secret = "SYNTHETIC_SYSTEM_SECRET=private-context-never-log";
        let dynamic_secret = "SYNTHETIC_DYNAMIC_SECRET=tool-context-never-log";
        let buf = BufWriter::default();
        let writer = buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let sid = "drift-private-prompt-test-session";
            observe(sid, &["base-private-prompt"], true);
            observe(sid, &["base-private-prompt", stable_secret], true);
            log_diverted(sid, &[dynamic_secret]);
        });
        let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert_eq!(out.matches("[drift]").count(), 2, "{out}");
        for secret in [stable_secret, dynamic_secret] {
            assert!(
                out.contains(&format!("hash={:016x}", fingerprint(secret).0)),
                "{out}"
            );
        }
        for private_text in [
            "base-private-prompt",
            "SYNTHETIC_",
            "private-context-never-log",
            "tool-context-never-log",
        ] {
            assert!(!out.contains(private_text), "{out}");
        }
    }

    #[test]
    fn oversized_stable_blocks_skip_snapshot_and_debug_diff() {
        let sid = "drift-oversized-test-session";
        let blocks = vec!["SYNTHETIC_PRIVATE_OVERSIZED_BLOCK"; MAX_TRACKED_BLOCKS + 1];
        let buf = BufWriter::default();
        let writer = buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            observe(sid, &blocks, true);
            observe(sid, &blocks, true);
            if let Some(map) = PREV_BLOCKS.get() {
                assert!(!map.lock().unwrap().contains_key(sid));
            }
            assert!(buf.0.lock().unwrap().is_empty());

            // 超量请求不占用/污染会话快照；之后的小请求照常记录和比较。
            observe(sid, &["small baseline"], true);
            observe(sid, &["small baseline", "small addition"], true);
        });
        let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert_eq!(out.matches("[drift]").count(), 1, "{out}");
        assert!(out.contains("added=1"), "{out}");
        assert!(!out.contains("SYNTHETIC_PRIVATE"), "{out}");
    }
}
