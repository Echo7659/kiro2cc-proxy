// Copyright (c) 2026 Harllan He. Licensed under MIT.
//! Kiro 请求体落盘（dump）领域逻辑：路径、命名、校验、清单与滚动清理。
//!
//! 所有文件系统操作都参数化到 `dir`，使单测可注入临时目录；
//! 生产路径由无参包装 [`list_dump_files`] 指向 [`DUMP_DIR`]。

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// dump 目录（相对路径，真实位置由进程 CWD 决定：容器内 `/app`，systemd 下安装目录）。
pub const DUMP_DIR: &str = "app/config/kiro-body-dump";

/// 保留的 dump 文件上限。
///
/// 清理发生在写入之前（见 [`evict_for_incoming_in`]），且「腾位 + 写入」临界区由
/// [`WRITE_LOCK`] 串行化，故并发写入下目录内文件数仍恒 ≤ 本值。
pub const MAX_DUMP_FILES: usize = 200;

/// 归档总量上限（防御性上限：实测 200 文件约 34 MB，本值约为其 4 倍）。
///
/// 口径：对「条目数据 + 本地头 + 中央目录 + EOCD」为**严格保证**；
/// 截断时追加的 `README.txt` 说明条目不在判据内（其大小取决于被跳过条目数，
/// 最坏情形约 16 KiB），故最终归档字节数可能比本值大该量级。
///
/// 注意：ZIP 未实现 ZIP64，中央目录偏移以 u32 表示，本值必须 < 4 GiB 否则偏移溢出。
/// 调高前先评估 ZIP64 必要性（见 `src/admin/zip.rs`）。
pub const MAX_ARCHIVE_BYTES: u64 = 128 * 1024 * 1024;

// 编译期守卫：未实现 ZIP64，中央目录偏移以 u32 表示，上限必须 < 4 GiB 否则溢出
const _: () = assert!(MAX_ARCHIVE_BYTES < 4 * 1024 * 1024 * 1024);

/// 同一毫秒时间戳下的文件名冲突重试上限。
pub const COLLISION_RETRIES: usize = 5;

/// 串行化「腾位 + 写入」临界区。
///
/// 不串行时 N 个并发写入方可同时观察到 `len == MAX_DUMP_FILES - 1` 而都不腾位，
/// 落盘后数量可达 `MAX_DUMP_FILES - 1 + N`，突破 [`MAX_DUMP_FILES`] 上限不变量。
/// 调用点已处于 `spawn_blocking` 内，故此处使用阻塞锁即可。
static WRITE_LOCK: Mutex<()> = Mutex::new(());

/// dump 文件元数据。
///
/// `created_at_ms` 取自**文件名内时间戳**而非文件系统 mtime：后者在文件被
/// `cp` / `touch` / 跨文件系统拷贝后失真，且文件名口径与滚动清理的排序口径一致。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DumpFileMeta {
    pub name: String,
    pub size_bytes: u64,
    pub created_at_ms: u64,
}

/// 生成 dump 文件名：`body-<ts>.json`；冲突时 `body-<ts>-<attempt>.json`。
pub fn dump_file_name(ts_millis: u64, attempt: usize) -> String {
    if attempt == 0 {
        format!("body-{ts_millis}.json")
    } else {
        format!("body-{ts_millis}-{attempt}.json")
    }
}

/// 严格校验 dump 文件名（白名单字节扫描，不依赖 regex）。
///
/// 语义等价 `^body-\d+(-\d+)?\.json$`，并额外拒绝空串、非 ASCII 与超长名：
/// 非 ASCII 由 `is_ascii` 前置检查拒绝；分隔符 `/`、`\` 与 NUL 虽属 ASCII，
/// 但分别因不满足 `body-<数字>…` 模式或含非数字字节被 `all_digits` 拒绝。
pub fn is_valid_dump_file_name(name: &str) -> bool {
    const PREFIX: &str = "body-";
    const SUFFIX: &str = ".json";
    /// 生成名最长形态 `body-<13位>-<1位>.json` = 25 字节；取 48 留裕度并挡住超长名
    const MAX_NAME_LEN: usize = 48;

    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_NAME_LEN || !bytes.is_ascii() {
        return false;
    }
    let Some(mid) = name
        .strip_prefix(PREFIX)
        .and_then(|rest| rest.strip_suffix(SUFFIX))
    else {
        return false;
    };

    let all_digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());

    // mid 形如 `<ts>` 或 `<ts>-<attempt>`；出现第二个 '-' 即拒绝
    let mut parts = mid.split('-');
    let first = parts.next().unwrap_or("");
    let second = parts.next();
    if parts.next().is_some() {
        return false;
    }
    match second {
        None => all_digits(first),
        Some(attempt) => all_digits(first) && all_digits(attempt),
    }
}

/// 从文件名解析毫秒时间戳（碰撞后缀不影响解析）。格式非法返回 `None`。
pub fn dump_file_timestamp_ms(name: &str) -> Option<u64> {
    if !is_valid_dump_file_name(name) {
        return None;
    }
    let mid = name.strip_prefix("body-")?.strip_suffix(".json")?;
    mid.split('-').next()?.parse::<u64>().ok()
}

/// 枚举目录内符合命名规则的真实文件，按**文件名升序**返回 `(路径, 文件名)`。
///
/// 目录不存在时返回空 Vec（不视为错误）；目录存在但读取失败时告警一次并返回空。
/// 注意：该告警**仅在日志层**提供区分——Admin 列表端点在两种情形下都返回 200 + 空数组，
/// 有意保持该 HTTP 契约不变，避免前端把运维故障误报为请求失败。
///
/// [`list_dump_files_in`] 与 [`evict_for_incoming_in`] 共用本函数，
/// 使「列表」与「清理」的过滤谓词和排序口径同源，不会各自演进而漂移。
fn enumerate_valid(dir: &Path) -> Vec<(PathBuf, String)> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            tracing::warn!("[BODY-DUMP] 读取 dump 目录失败 {}: {}", dir.display(), e);
            return Vec::new();
        }
    };
    let mut out: Vec<(PathBuf, String)> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if !is_valid_dump_file_name(&name) {
                return None;
            }
            let path = e.path();
            path.is_file().then_some((path, name))
        })
        .collect();
    out.sort_by(|a, b| a.1.cmp(&b.1));
    out
}

/// 列出目录内符合命名规则的 dump 文件，按文件名排序。
///
/// 文件名为 13 位定长毫秒时间戳，故字典序等价于时间序。
/// 目录不存在或无匹配文件时返回空 Vec（不视为错误）。
pub fn list_dump_files_in(dir: &Path) -> Vec<DumpFileMeta> {
    enumerate_valid(dir)
        .into_iter()
        .filter_map(|(path, name)| {
            let size_bytes = path.metadata().ok()?.len();
            let created_at_ms = dump_file_timestamp_ms(&name)?;
            Some(DumpFileMeta {
                name,
                size_bytes,
                created_at_ms,
            })
        })
        .collect()
}

/// 生产包装：列出 [`DUMP_DIR`] 内的 dump 文件。
pub fn list_dump_files() -> Vec<DumpFileMeta> {
    list_dump_files_in(Path::new(DUMP_DIR))
}

/// 读取指定 dump 文件的原始字节。文件名非法或文件不存在返回 `None`。
pub fn read_dump_file_in(dir: &Path, name: &str) -> Option<Vec<u8>> {
    if !is_valid_dump_file_name(name) {
        return None;
    }
    std::fs::read(dir.join(name)).ok()
}

/// 为「即将写入的新文件」腾位：保留最新 `MAX_DUMP_FILES - 1` 个。
///
/// 语义等价于原实现（写入前腾位）：目录已有 200 个时删至 199，随后写入新文件，
/// 使落盘后数量恒 ≤ [`MAX_DUMP_FILES`]。
///
/// **不得改为通用的 keep-newest-N**：若在 `len == MAX_DUMP_FILES` 时删 0 个，
/// 写入后会短暂出现 201 个，破坏上限不变量。
///
/// 过滤谓词与 [`list_dump_files_in`] 同源（同一 `is_valid_dump_file_name`），
/// 故手工放入的非本功能命名的 `.json` 既不占名额也不被删除。
pub fn evict_for_incoming_in(dir: &Path) {
    // 排序口径与 `list_dump_files_in` 同源（均按文件名升序），故删除的必然是最旧的一个
    let entries = enumerate_valid(dir);
    if entries.len() < MAX_DUMP_FILES {
        return;
    }
    let excess = entries.len() - MAX_DUMP_FILES + 1;
    for (stale, _) in &entries[..excess] {
        if let Err(e) = std::fs::remove_file(stale) {
            tracing::warn!(
                "[BODY-DUMP] 清理旧 dump 文件失败 {}: {}",
                stale.display(),
                e
            );
        }
    }
}

/// 向刚创建的文件写入内容；写入失败时删除残留文件，避免半个文件占用 [`MAX_DUMP_FILES`] 名额。
///
/// 先 `drop` 句柄再删除：Windows 上删除仍处于打开状态的文件会失败。
/// 抽为独立函数是为了让「写失败 → 清理」这条分支能在单测中直接注入失败句柄覆盖。
fn write_all_or_cleanup(mut file: std::fs::File, path: &Path, body: &str) -> std::io::Result<()> {
    use std::io::Write;

    match file.write_all(body.as_bytes()) {
        Ok(()) => Ok(()),
        Err(e) => {
            drop(file);
            let _ = std::fs::remove_file(path);
            Err(e)
        }
    }
}

/// 将请求体写入 dump 目录：0600 权限，写入前腾位，文件名冲突时按 [`dump_file_name`] 重试。
///
/// 返回实际写入路径。创建目录失败或写入失败返回 `Err`。
pub fn write_dump_body_in(dir: &Path, body: &str, ts_millis: u64) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    // 中毒锁取回内层：临界区只做文件系统序列化，不携带可被污染的共享状态，
    // 持锁期间 panic 不应永久阻断后续 dump 写入
    let _guard = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    evict_for_incoming_in(dir);

    let mut open = std::fs::OpenOptions::new();
    open.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open.mode(0o600);
    }

    // 共 COLLISION_RETRIES + 1 次尝试（含首次）
    for attempt in 0..=COLLISION_RETRIES {
        let path = dir.join(dump_file_name(ts_millis, attempt));
        let file = match open.open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        write_all_or_cleanup(file, &path, body)?;
        return Ok(path);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!("文件名冲突重试 {COLLISION_RETRIES} 次仍失败"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用临时目录：系统临时目录 + tag + 进程 pid，避免并行测试互相干扰
    /// （对齐 `src/admin/handlers.rs::temp_config_dir` 先例）
    fn temp_dump_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("kiro-dump-test-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn test_dump_file_name_format() {
        assert_eq!(dump_file_name(1700000000000, 0), "body-1700000000000.json");
        assert_eq!(
            dump_file_name(1700000000000, 3),
            "body-1700000000000-3.json"
        );
    }

    #[test]
    fn test_is_valid_dump_file_name_accepts() {
        for name in [
            "body-1700000000000.json",
            "body-1700000000000-3.json",
            "body-1.json",
            "body-1-1.json",
        ] {
            assert!(is_valid_dump_file_name(name), "{name} 应被接受");
        }
    }

    #[test]
    fn test_is_valid_dump_file_name_rejects() {
        let long_name = format!("body-{}.json", "1".repeat(60));
        for name in [
            "",
            "body-.json",
            "body-x.json",
            "body-1-.json",
            "body--1.json",
            "body-1-2-3.json",
            "notes.txt",
            "notes.json",
            "../config.json",
            "..%2Fconfig.json",
            "sub/body-1.json",
            "body-1.json\0",
            "backup.json",
            "Body-1.json",
            "body-1.JSON",
            "body-1é.json",
            long_name.as_str(),
        ] {
            assert!(!is_valid_dump_file_name(name), "{name:?} 应被拒绝");
        }
    }

    #[test]
    fn test_dump_file_timestamp_ms() {
        assert_eq!(
            dump_file_timestamp_ms("body-1700000000000.json"),
            Some(1700000000000)
        );
        // 碰撞后缀不影响时间戳解析
        assert_eq!(
            dump_file_timestamp_ms("body-1700000000000-3.json"),
            Some(1700000000000)
        );
        assert_eq!(dump_file_timestamp_ms("notes.json"), None);
    }

    #[test]
    fn test_list_dump_files_filters_and_sorts() {
        let dir = temp_dump_dir("list");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("body-1700000001000.json"), b"bb").unwrap();
        std::fs::write(dir.join("body-1700000000000.json"), b"a").unwrap();
        std::fs::write(dir.join("body-1700000000000-3.json"), b"ccc").unwrap();
        // 非本功能命名的文件必须被过滤
        std::fs::write(dir.join("notes.json"), b"x").unwrap();
        std::fs::write(dir.join("notes.txt"), b"x").unwrap();

        let files = list_dump_files_in(&dir);
        assert_eq!(files.len(), 3, "应只列出符合命名规则的 3 个文件");
        assert_eq!(
            files.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
            vec![
                "body-1700000000000-3.json",
                "body-1700000000000.json",
                "body-1700000001000.json"
            ]
        );
        assert_eq!(files[0].created_at_ms, 1700000000000);
        assert_eq!(files[0].size_bytes, 3);
        assert_eq!(files[2].created_at_ms, 1700000001000);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_list_dump_files_missing_dir_is_empty() {
        let dir = temp_dump_dir("missing");
        assert!(list_dump_files_in(&dir).is_empty());
    }

    #[test]
    fn test_evict_for_incoming_removes_one_at_capacity() {
        let dir = temp_dump_dir("evict");
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..MAX_DUMP_FILES {
            std::fs::write(dir.join(format!("body-{i:013}.json")), b"x").unwrap();
        }
        // 手工放入的非本功能命名文件不占名额、不被删除
        std::fs::write(dir.join("notes.json"), b"x").unwrap();
        assert_eq!(list_dump_files_in(&dir).len(), MAX_DUMP_FILES);

        evict_for_incoming_in(&dir);

        let after = list_dump_files_in(&dir);
        assert_eq!(
            after.len(),
            MAX_DUMP_FILES - 1,
            "满额时应删至 MAX_DUMP_FILES - 1，为即将写入的文件腾位"
        );
        assert_eq!(after[0].name, "body-0000000000001.json", "应删除最旧的一个");
        assert!(
            dir.join("notes.json").exists(),
            "非本功能命名的文件不应被删除"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_evict_for_incoming_is_noop_below_capacity() {
        let dir = temp_dump_dir("evict-noop");
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..MAX_DUMP_FILES - 1 {
            std::fs::write(dir.join(format!("body-{i:013}.json")), b"x").unwrap();
        }
        evict_for_incoming_in(&dir);
        assert_eq!(list_dump_files_in(&dir).len(), MAX_DUMP_FILES - 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_dump_body_keeps_count_within_limit() {
        let dir = temp_dump_dir("write-cap");
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..MAX_DUMP_FILES {
            std::fs::write(dir.join(format!("body-{i:013}.json")), b"x").unwrap();
        }

        let path = write_dump_body_in(&dir, "hello", 1700000009999).unwrap();
        assert!(path.exists());

        // 上限不变量：写入后数量恒 ≤ MAX_DUMP_FILES
        let count = list_dump_files_in(&dir).len();
        assert!(
            count <= MAX_DUMP_FILES,
            "写入后文件数 {count} 应 ≤ {MAX_DUMP_FILES}"
        );
        assert_eq!(count, MAX_DUMP_FILES);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_dump_body_evicts_oldest_not_foreign_json() {
        let dir = temp_dump_dir("write-evict");
        std::fs::create_dir_all(&dir).unwrap();
        // 满额 200 个 body-* + 1 个手工放入的 notes.json
        for i in 0..MAX_DUMP_FILES {
            std::fs::write(dir.join(format!("body-{i:013}.json")), b"x").unwrap();
        }
        std::fs::write(dir.join("notes.json"), b"keep-me").unwrap();

        write_dump_body_in(&dir, "fresh", 1700000009999).unwrap();

        let files = list_dump_files_in(&dir);
        assert_eq!(files.len(), MAX_DUMP_FILES, "落盘后仍应保持满额");
        // 被删除的是最早的 body-*
        assert_eq!(files[0].name, "body-0000000000001.json");
        // 新文件已在列
        assert!(files.iter().any(|f| f.name == "body-1700000009999.json"));
        // notes.json 未被删除、且不出现在列表中
        assert_eq!(
            std::fs::read(dir.join("notes.json")).unwrap(),
            b"keep-me",
            "非本功能命名的文件不应被删除"
        );
        assert!(!files.iter().any(|f| f.name == "notes.json"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_dump_body_collision_suffix() {
        let dir = temp_dump_dir("write-collision");
        std::fs::create_dir_all(&dir).unwrap();

        let ts = 1700000000000;
        let first = write_dump_body_in(&dir, "a", ts).unwrap();
        let second = write_dump_body_in(&dir, "b", ts).unwrap();
        assert_eq!(first.file_name().unwrap(), "body-1700000000000.json");
        assert_eq!(second.file_name().unwrap(), "body-1700000000000-1.json");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_dump_body_concurrent_never_exceeds_limit() {
        let dir = temp_dump_dir("write-concurrent");
        std::fs::create_dir_all(&dir).unwrap();
        // 预置到「只差 1 个满额」：并发下若不串行腾位，各写入方都看不到满额即都不腾位
        for i in 0..MAX_DUMP_FILES - 1 {
            std::fs::write(dir.join(format!("body-{i:013}.json")), b"x").unwrap();
        }

        let handles: Vec<_> = (0..20)
            .map(|n| {
                let dir = dir.clone();
                std::thread::spawn(move || {
                    write_dump_body_in(&dir, "payload", 1700000000000 + n).unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let count = list_dump_files_in(&dir).len();
        assert!(
            count <= MAX_DUMP_FILES,
            "并发写入后文件数 {count} 应 ≤ {MAX_DUMP_FILES}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_valid_names_cover_historical_write_format() {
        // 历史写入路径只产生 body-<ts>.json 与 body-<ts>-<attempt>.json（attempt ∈ 1..=COLLISION_RETRIES）。
        // 二者均通过新谓词，故不存在「历史文件不被新谓词识别、因而永不参与清理」的残留
        assert!(is_valid_dump_file_name(&dump_file_name(1700000000000, 0)));
        for attempt in 1..=COLLISION_RETRIES {
            assert!(
                is_valid_dump_file_name(&dump_file_name(1700000000000, attempt)),
                "attempt={attempt} 的历史命名应被新谓词接受"
            );
        }
    }

    #[test]
    fn test_write_all_failure_removes_partial_file() {
        let dir = temp_dump_dir("write-fail");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("body-1700000000000.json");
        std::fs::write(&path, b"").unwrap();

        // 以只读句柄作为 `write_all_or_cleanup` 的形参：写入必然失败，
        // 覆盖「create_new 成功但 write_all 失败」时删除残留的清理路径
        let file = std::fs::File::open(&path).unwrap();
        // 错误 kind 随平台而异（macOS 下只读句柄写入为 EBADF，映射到 Uncategorized，
        // 而非 PermissionDenied），故此处只断言「确实失败」，不锁定具体 kind
        assert!(
            write_all_or_cleanup(file, &path, "payload").is_err(),
            "向只读句柄写入应失败"
        );
        assert!(
            !path.exists(),
            "写失败后必须删除残留文件，避免占用 MAX_DUMP_FILES 名额"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn test_write_dump_body_permissions_0600() {
        use std::os::unix::fs::PermissionsExt;

        let dir = temp_dump_dir("write-perm");
        let path = write_dump_body_in(&dir, "secret", 1700000000000).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "dump 文件权限应为 0600");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_read_dump_file_in() {
        let dir = temp_dump_dir("read");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("body-1700000000000.json"), b"payload").unwrap();

        assert_eq!(
            read_dump_file_in(&dir, "body-1700000000000.json").as_deref(),
            Some(b"payload".as_slice())
        );
        // 合法名但文件不存在
        assert_eq!(read_dump_file_in(&dir, "body-1700000009999.json"), None);
        // 非法名即使磁盘上真有同名文件也读不到
        std::fs::write(dir.join("notes.json"), b"x").unwrap();
        assert_eq!(read_dump_file_in(&dir, "notes.json"), None);
        assert_eq!(read_dump_file_in(&dir, "../config.json"), None);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
