// Copyright (c) 2026 Harllan He. Licensed under MIT.
//! Kiro 请求体落盘（dump）文件的浏览与下载 handler。
//!
//! 三个端点均挂载在 `create_admin_router` 的 protected 子路由下，
//! 由 `admin_auth_middleware` 统一鉴权，故此处不再重复校验 admin key。
//!
//! - `GET /api/admin/dump-kiro-body/files`          列表
//! - `GET /api/admin/dump-kiro-body/files/{name}`   单文件下载
//! - `GET /api/admin/dump-kiro-body/archive`        全量打包（STORED ZIP，流式）

use axum::{
    body::Body,
    extract::Path,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures::{StreamExt, stream};
use std::path::{Path as FsPath, PathBuf};

use super::types::DumpFilesResponse;
use super::zip::{self, CentralEntry};
use crate::common::dump::{self, DUMP_DIR, MAX_ARCHIVE_BYTES, MAX_DUMP_FILES};

/// 截断或流中缺失时追加的说明文件名（计入归档条目总数）
const README_ENTRY: &str = "README.txt";

/// 在阻塞线程池内列出指定目录的 dump 文件（参数化以便单测注入临时目录）。
///
/// 目录扫描是同步磁盘 IO，直接放在 async 上下文会阻塞 tokio worker 线程，
/// 影响同进程其余代理请求（本项目为高并发服务）。任务 panic 时返回 `None`。
#[cfg(test)]
async fn list_dump_files_blocking_in(dir: PathBuf) -> Option<Vec<dump::DumpFileMeta>> {
    tokio::task::spawn_blocking(move || dump::list_dump_files_in(&dir))
        .await
        .ok()
}

/// 生产包装：在阻塞线程池内列出 [`DUMP_DIR`]。
async fn list_dump_files_blocking() -> Option<Vec<dump::DumpFileMeta>> {
    tokio::task::spawn_blocking(dump::list_dump_files)
        .await
        .ok()
}

/// 列表结果 → 响应：`None`（阻塞任务 panic）→ 500，`Some` → 200 + JSON
fn list_response(files: Option<Vec<dump::DumpFileMeta>>) -> Response {
    match files {
        Some(files) => respond_with_list(files),
        None => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// 单文件下载结果 → 响应：`None`（阻塞任务 panic）→ 500，`Some` 按文件系统结果分流
fn download_response(outcome: Option<(String, Result<Vec<u8>, StatusCode>)>) -> Response {
    match outcome {
        Some((name, Ok(bytes))) => file_attachment_response(&name, bytes),
        Some((_, Err(status))) => status.into_response(),
        None => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// 归档结果 → 响应：`None`（阻塞任务 panic）→ 500，`Some` 交给 [`archive_response_for`]
fn archive_response(dir: PathBuf, files: Option<Vec<dump::DumpFileMeta>>) -> Response {
    match files {
        Some(files) => archive_response_for(dir, files),
        None => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// 在阻塞线程池内读取单个 dump 文件；任务 panic 或文件缺失均返回 `None`。
async fn read_dump_file_blocking(dir: &FsPath, name: &str) -> Option<Vec<u8>> {
    let dir = dir.to_path_buf();
    let name = name.to_string();
    tokio::task::spawn_blocking(move || dump::read_dump_file_in(&dir, &name))
        .await
        .ok()
        .flatten()
}

/// GET /api/admin/dump-kiro-body/files
///
/// 返回 dump 目录内符合命名规则的文件元数据；目录不存在或为空时返回空数组而非错误，
/// 使前端无需区分「开关未开启」与「请求失败」。
pub async fn list_dump_files_handler() -> Response {
    list_response(list_dump_files_blocking().await)
}

/// 由文件清单构造响应（抽出以便单测直接注入清单）
fn respond_with_list(files: Vec<dump::DumpFileMeta>) -> Response {
    let total_bytes = files.iter().map(|f| f.size_bytes).sum();
    axum::Json(DumpFilesResponse {
        files,
        total_bytes,
        max_files: MAX_DUMP_FILES,
        max_archive_bytes: MAX_ARCHIVE_BYTES,
    })
    .into_response()
}

/// GET /api/admin/dump-kiro-body/files/{name}
///
/// 先经 [`dump::is_valid_dump_file_name`] 白名单校验（不合法 → 400），
/// 再拼接到 dump 目录读取（不存在 → 404）。错误响应不含服务端绝对路径。
pub async fn download_dump_file(Path(name): Path<String>) -> Response {
    let dir = PathBuf::from(DUMP_DIR);
    let task = tokio::task::spawn_blocking(move || {
        let outcome = read_dump_file_for_download(&dir, &name);
        (name, outcome)
    })
    .await;

    if let Err(e) = &task {
        tracing::warn!("[BODY-DUMP] 单文件下载任务失败: {}", e);
    }
    download_response(task.ok())
}

/// 校验 + 读取（抽出以便单测以纯函数方式覆盖 400 / 404 两条分支）
///
/// 返回 `Err(400)` 表示文件名非法（**不触达文件系统**），`Err(404)` 表示文件不存在。
fn read_dump_file_for_download(dir: &FsPath, name: &str) -> Result<Vec<u8>, StatusCode> {
    if !dump::is_valid_dump_file_name(name) {
        return Err(StatusCode::BAD_REQUEST);
    }
    dump::read_dump_file_in(dir, name).ok_or(StatusCode::NOT_FOUND)
}

/// 构造单文件下载响应：原始字节 + 附件头 + nosniff
fn file_attachment_response(name: &str, bytes: Vec<u8>) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    // 文件名来自白名单校验后的 `body-<数字>[-<数字>].json`，不含引号等需转义字符
    let disposition = format!("attachment; filename=\"{name}\"");
    headers.insert(
        header::CONTENT_DISPOSITION,
        header::HeaderValue::from_str(&disposition)
            .unwrap_or_else(|_| header::HeaderValue::from_static("attachment")),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
    // dump 正文含完整上游请求体（prompt、系统提示等敏感内容），显式禁缓存
    headers.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    (headers, bytes).into_response()
}

/// GET /api/admin/dump-kiro-body/archive
///
/// 全量打包为 STORED ZIP 并以流式响应输出。
///
/// 输出前先做列表预检：无可纳入文件时返回 404（状态码在响应体开始前确定，
/// 不会出现「先 200 后中断」）。逐个文件流式产出，内存持有量为
/// 「当前文件字节 + 中央目录元数据」，不累积已发送文件的数据字节。
pub async fn download_dump_archive() -> Response {
    archive_response(PathBuf::from(DUMP_DIR), list_dump_files_blocking().await)
}

/// 由 dump 目录与文件清单构造归档响应
/// （抽出以便单测注入临时目录与清单，无需触碰生产 [`DUMP_DIR`]）
fn archive_response_for(dir: PathBuf, files: Vec<dump::DumpFileMeta>) -> Response {
    if files.is_empty() {
        return StatusCode::NOT_FOUND.into_response();
    }

    let timestamp = chrono::Utc::now().format("%Y%m%d-%H%M%S").to_string();
    let disposition = format!("attachment; filename=\"kiro-body-dump-{timestamp}.zip\"");

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/zip"),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        header::HeaderValue::from_str(&disposition)
            .unwrap_or_else(|_| header::HeaderValue::from_static("attachment")),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
    // 归档内容同样含完整上游请求体，显式禁缓存
    headers.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );

    let body = Body::from_stream(stream_archive(dir, files));
    (headers, body).into_response()
}

/// 归档流式产出的状态：目录、待处理文件、已写入偏移、中央目录累积、说明文字行
struct ArchiveState {
    dir: PathBuf,
    pending: Vec<dump::DumpFileMeta>,
    max_bytes: u64,
    offset: u32,
    /// 已累积的中央目录字节数（每条 = `CENTRAL_ENTRY_LEN` + 文件名长度）
    central_len: u32,
    central: Vec<CentralEntry>,
    notes: Vec<String>,
    truncated: bool,
}

/// 逐文件流式产出归档字节；末尾输出中央目录 + EOCD（生产上限 [`MAX_ARCHIVE_BYTES`]）。
fn stream_archive(
    dir: PathBuf,
    files: Vec<dump::DumpFileMeta>,
) -> impl futures::Stream<Item = Result<Vec<u8>, std::io::Error>> {
    stream_archive_with_limit(dir, files, MAX_ARCHIVE_BYTES)
}

/// 取出下一个可纳入条目并产出其 chunk；耗尽或全部被跳过时返回 `None`。
///
/// 判据使用**实际读取长度**（先读后判），而非列表元数据中的 `size_bytes`：
/// 列表与读取之间存在文件被追加写的窗口，用元数据会越过上限。
/// 判据同时计入本条中央目录增量与 EOCD，故上限对
/// 「数据 + 本地头 + 中央目录 + EOCD」严格成立（README 条目见 [`take_notes_chunk`]）。
/// 纳入一个条目后归档总字节数的算式：
/// `当前偏移 + 本地头 + 数据 + 已累积中央目录 + 本条中央目录增量 + EOCD`。
///
/// `data_len` 传列表元数据 `size_bytes` 时得到保守**下界**（快筛用）；
/// 传实际读取长度时得到**真值**（终判用）。两处判据共用本式，避免口径漂移。
fn projected_total(st: &ArchiveState, name: &str, data_len: u64) -> u64 {
    st.offset as u64
        + (zip::LOCAL_HEADER_LEN + name.len()) as u64
        + data_len
        + st.central_len as u64
        + (zip::CENTRAL_ENTRY_LEN + name.len()) as u64
        + zip::EOCD_LEN as u64
}

/// 记录「加入后超出归档总量上限，整文件跳过」：置截断标记并追加说明行
fn skip_over_limit(st: &mut ArchiveState, name: &str) {
    st.truncated = true;
    st.notes.push(format!(
        "{}: 跳过（加入后超出归档总量上限 {} 字节）",
        name, st.max_bytes
    ));
}

async fn take_next_entry_chunk(st: &mut ArchiveState) -> Option<Vec<u8>> {
    while let Some(meta) = st.pending.pop() {
        // 快筛：以列表元数据 `size_bytes` 估出的最小可能开销已超上限时无需读盘。
        // 实际长度 ≥ 列表时的 `size_bytes`（文件只会被追加，不会被截断），
        // 故该下界保守成立；唯一偏差窗口是「列表后文件被截短」，
        // 其时快筛偏大而跳过文件——方向安全（不会越过上限）。
        if projected_total(st, &meta.name, meta.size_bytes) > st.max_bytes {
            skip_over_limit(st, &meta.name);
            continue;
        }

        let Some(data) = read_dump_file_blocking(&st.dir, &meta.name).await else {
            st.notes
                .push(format!("{}: 跳过（读取失败，可能已被滚动清理）", meta.name));
            continue;
        };

        // 整文件粒度：加入后超限则整个跳过，保留最新取证数据
        if projected_total(st, &meta.name, data.len() as u64) > st.max_bytes {
            skip_over_limit(st, &meta.name);
            continue;
        }

        let crc = zip::entry_crc(&data);
        let size = zip::as_u32_len(data.len());
        st.central.push(CentralEntry {
            name: meta.name.clone(),
            crc,
            size,
            local_header_offset: st.offset,
        });
        st.central_len += zip::as_u32_len(zip::CENTRAL_ENTRY_LEN + meta.name.len());

        let mut chunk = zip::local_header(&meta.name, crc, size);
        st.offset += zip::as_u32_len(chunk.len() + data.len());
        chunk.extend_from_slice(&data);
        return Some(chunk);
    }
    None
}

/// 文件耗尽后追加说明条目（如有）。返回 `None` 表示本轮无需追加。
///
/// 必须先 `take` 走 notes，否则下一轮轮询会再次命中本分支、重复产出说明段。
fn take_notes_chunk(st: &mut ArchiveState) -> Option<Vec<u8>> {
    if st.notes.is_empty() {
        return None;
    }

    let notes = std::mem::take(&mut st.notes);
    let data = render_notes(&notes, st.truncated).into_bytes();
    let crc = zip::entry_crc(&data);
    let size = zip::as_u32_len(data.len());
    st.central.push(CentralEntry {
        name: README_ENTRY.to_string(),
        crc,
        size,
        local_header_offset: st.offset,
    });
    st.central_len += zip::as_u32_len(zip::CENTRAL_ENTRY_LEN + README_ENTRY.len());

    let mut chunk = zip::local_header(README_ENTRY, crc, size);
    st.offset += zip::as_u32_len(chunk.len() + data.len());
    chunk.extend_from_slice(&data);
    Some(chunk)
}

/// 收尾：中央目录 + EOCD
fn finish_archive(st: &ArchiveState) -> Vec<u8> {
    let mut out = Vec::with_capacity(st.central_len as usize + zip::EOCD_LEN);
    for entry in &st.central {
        out.extend_from_slice(&zip::central_entry(entry));
    }
    let cd_size = zip::as_u32_len(out.len());
    out.extend_from_slice(&zip::eocd(st.central.len(), cd_size, st.offset));
    out
}

/// [`stream_archive`] 的参数化实现（抽出 `max_bytes` 以便单测在极小数据量下覆盖截断分支）。
///
/// 累积方向为**从最新到最旧**（`files` 按名升序，`pop()` 自尾部取出），
/// 使截断时保留最新取证数据。文件按**整文件**粒度纳入或跳过。
/// 流中读取失败（并发滚动清理等）同样跳过并记录，不中断流。
fn stream_archive_with_limit(
    dir: PathBuf,
    files: Vec<dump::DumpFileMeta>,
    max_bytes: u64,
) -> impl futures::Stream<Item = Result<Vec<u8>, std::io::Error>> {
    stream::unfold(
        Some(ArchiveState {
            dir,
            // `files` 按名升序（= 时间升序），`pop()` 从尾部取出即为**最新优先**
            pending: files,
            max_bytes,
            offset: 0,
            central_len: 0,
            central: Vec::new(),
            notes: Vec::new(),
            truncated: false,
        }),
        |state| async move {
            let mut st = state?;

            // 数据条目 → 说明条目 → 收尾，三段各产出一次 chunk后就地交回状态
            if let Some(chunk) = take_next_entry_chunk(&mut st).await {
                return Some((Ok(chunk), Some(st)));
            }
            if let Some(chunk) = take_notes_chunk(&mut st) {
                return Some((Ok(chunk), Some(st)));
            }
            // 最后一段发出后结束流
            Some((Ok(finish_archive(&st)), None))
        },
    )
    .boxed()
}

/// 渲染归档内说明文件正文（截断与跳过记录）
fn render_notes(notes: &[String], truncated: bool) -> String {
    let mut text = String::from("# kiro2cc-proxy dump archive notes\n");
    text.push_str(&format!("truncated: {truncated}\n"));
    for line in notes {
        text.push_str(line);
        text.push('\n');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dump_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kiro-dump-handler-test-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// 把流式产出的所有 chunk 拼接为完整字节
    async fn collect_archive(dir: PathBuf, files: Vec<dump::DumpFileMeta>) -> Vec<u8> {
        collect_archive_with_limit(dir, files, MAX_ARCHIVE_BYTES).await
    }

    /// 同上，但以指定归档上限跑（用于在极小数据量下覆盖截断分支）
    async fn collect_archive_with_limit(
        dir: PathBuf,
        files: Vec<dump::DumpFileMeta>,
        max_bytes: u64,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        let mut s = Box::pin(stream_archive_with_limit(dir, files, max_bytes));
        while let Some(item) = s.next().await {
            out.extend_from_slice(&item.expect("归档流不应产生 IO 错误"));
        }
        out
    }

    #[tokio::test]
    async fn test_list_handler_empty_dir_returns_empty_list() {
        let resp = respond_with_list(Vec::new());
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["files"].as_array().unwrap().len(), 0);
        assert_eq!(json["totalBytes"], 0);
    }

    #[tokio::test]
    async fn test_list_handler_filters_and_sums() {
        let dir = temp_dump_dir("list");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("body-1700000000000.json"), vec![b'a'; 1024]).unwrap();
        std::fs::write(dir.join("body-1700000001000.json"), vec![b'b'; 2048]).unwrap();
        // 非本功能命名的文件必须被过滤
        std::fs::write(dir.join("notes.json"), b"x").unwrap();

        let files = dump::list_dump_files_in(&dir);
        let resp = respond_with_list(files);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(
            json["files"].as_array().unwrap().len(),
            2,
            "应过滤掉 notes.json"
        );
        assert_eq!(json["totalBytes"], 3072);
        assert_eq!(json["maxFiles"], MAX_DUMP_FILES);
        assert_eq!(json["maxArchiveBytes"], MAX_ARCHIVE_BYTES);
        // 排序正确（按名 = 按时间）
        assert_eq!(json["files"][0]["name"], "body-1700000000000.json");
        assert_eq!(json["files"][1]["name"], "body-1700000001000.json");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_download_file_rejects_invalid_names() {
        let dir = temp_dump_dir("dl-bad");
        std::fs::create_dir_all(&dir).unwrap();
        // 可达的恶意输入：HTTP 层能送达 handler 的形态
        for name in [
            "../config.json",
            "..%2Fconfig.json",
            "body-x.json",
            "notes.json",
            "body-1.json.evil",
        ] {
            assert_eq!(
                read_dump_file_for_download(&dir, name),
                Err(StatusCode::BAD_REQUEST),
                "{name} 应返回 400 且不触达文件系统"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_download_file_defensive_depth_names_rejected() {
        // 防御深度：这两类输入经 HTTP 层实际进不到 handler
        // （`{name}` 是单个路径段不含 `/`；NUL 无法出现在合法 URI 中），
        // 但在纯函数层仍必须被拒，避免将来调用方式变化时成为漏洞。
        let dir = temp_dump_dir("dl-defensive");
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["sub/body-1.json", "body-1.json\0", "body-1é.json"] {
            assert_eq!(
                read_dump_file_for_download(&dir, name),
                Err(StatusCode::BAD_REQUEST),
                "{name:?} 应在纯函数层被拒"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_download_file_missing_returns_404() {
        let dir = temp_dump_dir("dl-404");
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(
            read_dump_file_for_download(&dir, "body-1700000009999.json"),
            Err(StatusCode::NOT_FOUND)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_download_file_returns_exact_bytes_and_headers() {
        let dir = temp_dump_dir("dl-ok");
        std::fs::create_dir_all(&dir).unwrap();
        let payload = br#"{"hello":"dump"}"#;
        std::fs::write(dir.join("body-1700000000000.json"), payload).unwrap();

        let bytes =
            read_dump_file_for_download(&dir, "body-1700000000000.json").expect("应读取成功");
        assert_eq!(bytes.as_slice(), payload, "响应体应与磁盘字节一致");

        let resp = file_attachment_response("body-1700000000000.json", bytes);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
        assert_eq!(
            resp.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store",
            "dump 正文含敏感请求体，必须禁缓存"
        );
        assert_eq!(
            resp.headers().get(header::X_CONTENT_TYPE_OPTIONS).unwrap(),
            "nosniff"
        );
        let cd = resp
            .headers()
            .get(header::CONTENT_DISPOSITION)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(cd.starts_with("attachment"));
        assert!(cd.contains("body-1700000000000.json"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_archive_stream_matches_batch_build_and_sources() {
        let dir = temp_dump_dir("arch-match");
        std::fs::create_dir_all(&dir).unwrap();
        let entries: Vec<(String, Vec<u8>)> = vec![
            ("body-1700000000000.json".into(), b"aaa".to_vec()),
            ("body-1700000001000.json".into(), b"bbbb".to_vec()),
            ("body-1700000002000.json".into(), b"c".to_vec()),
        ];
        for (name, data) in &entries {
            std::fs::write(dir.join(name), data).unwrap();
        }

        let files = dump::list_dump_files_in(&dir);
        // 截断方向为从新到旧，故流内条目顺序与列表（升序）相反
        let mut expected = entries.clone();
        expected.reverse();

        let streamed = collect_archive(dir.clone(), files).await;
        let batched = zip::build_archive(&expected);

        assert_eq!(
            streamed, batched,
            "流式结果应与共用 helper 的批量构造逐字节一致"
        );

        // 解出归档内容，确认数据段与源文件逐字节一致
        let recovered = unzip_stored(&streamed);
        assert_eq!(recovered.len(), 3);
        for (name, data) in &entries {
            assert_eq!(
                recovered.get(name).map(|v| v.as_slice()),
                Some(data.as_slice())
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_blocking_list_wrapper_missing_dir_is_empty() {
        let dir = temp_dump_dir("blk-list-missing");
        let got = list_dump_files_blocking_in(dir.clone()).await;
        assert_eq!(
            got.map(|v| v.len()),
            Some(0),
            "目录不存在时应返回空清单而非 None"
        );
    }

    #[tokio::test]
    async fn test_blocking_list_wrapper_lists_and_filters() {
        let dir = temp_dump_dir("blk-list");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("body-1700000000000.json"), b"a").unwrap();
        std::fs::write(dir.join("notes.json"), b"x").unwrap();

        let got = list_dump_files_blocking_in(dir.clone()).await.unwrap();
        assert_eq!(got.len(), 1, "外来命名文件应被过滤");
        assert_eq!(got[0].name, "body-1700000000000.json");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_blocking_read_wrapper() {
        let dir = temp_dump_dir("blk-read");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("body-1700000000000.json"), b"payload").unwrap();

        assert_eq!(
            read_dump_file_blocking(&dir, "body-1700000000000.json")
                .await
                .as_deref(),
            Some(b"payload".as_slice())
        );
        assert_eq!(
            read_dump_file_blocking(&dir, "body-1700000009999.json").await,
            None
        );
        // 非法名不触达文件系统
        assert_eq!(read_dump_file_blocking(&dir, "../config.json").await, None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_response_mappers_cover_500_branches() {
        // 三条映射在 `None`（阻塞任务 panic）分支必须统一降级为 500
        assert_eq!(
            list_response(None).status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            download_response(None).status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            archive_response(temp_dump_dir("blk-500"), None).status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        // 对照：Some 分支不误降级
        assert_eq!(list_response(Some(Vec::new())).status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_archive_fast_filter_skips_without_reading() {
        let dir = temp_dump_dir("arch-fastfilter");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("body-1700000000000.json"), b"a").unwrap();
        std::fs::write(dir.join("body-1700000001000.json"), b"b").unwrap();

        let files = dump::list_dump_files_in(&dir);
        // 上限小于单条下界（145）：两个文件都应被快筛跳过，且仍产出说明文件
        let streamed = collect_archive_with_limit(dir.clone(), files, 144).await;
        let recovered = unzip_stored(&streamed);
        assert!(!recovered.contains_key("body-1700000000000.json"));
        assert!(!recovered.contains_key("body-1700000001000.json"));
        let notes = String::from_utf8_lossy(recovered.get(README_ENTRY).expect("应有说明文件"));
        assert!(notes.contains("truncated: true"), "{notes}");

        // 反向：上限恰好等于单条下界 → 最新一个应被纳入
        let files = dump::list_dump_files_in(&dir);
        let streamed = collect_archive_with_limit(dir.clone(), files, 145).await;
        let recovered = unzip_stored(&streamed);
        assert_eq!(
            recovered
                .get("body-1700000001000.json")
                .map(|v| v.as_slice()),
            Some(b"b".as_slice())
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_archive_response_empty_dir_returns_404() {
        let dir = temp_dump_dir("arch-empty");
        std::fs::create_dir_all(&dir).unwrap();
        // 目录存在但无匹配文件（如仅有外来 notes.json）同样视为「无可纳入文件」
        std::fs::write(dir.join("notes.json"), b"x").unwrap();

        let files = dump::list_dump_files_in(&dir);
        let resp = archive_response_for(dir.clone(), files);
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "无可纳入文件时必须在响应体开始前返回 404"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_archive_response_headers() {
        let dir = temp_dump_dir("arch-headers");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("body-1700000000000.json"), b"a").unwrap();

        let files = dump::list_dump_files_in(&dir);
        let resp = archive_response_for(dir.clone(), files);
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/zip"
        );
        assert_eq!(
            resp.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store",
            "归档含敏感请求体，必须禁缓存"
        );
        assert_eq!(
            resp.headers().get(header::X_CONTENT_TYPE_OPTIONS).unwrap(),
            "nosniff"
        );
        let cd = resp
            .headers()
            .get(header::CONTENT_DISPOSITION)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            cd.starts_with("attachment; filename=\"kiro-body-dump-"),
            "{cd}"
        );
        assert!(cd.ends_with(".zip\""), "{cd}");

        // 响应体应可被独立解析器完整还原
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let recovered = unzip_stored(&body);
        assert_eq!(
            recovered
                .get("body-1700000000000.json")
                .map(|v| v.as_slice()),
            Some(b"a".as_slice())
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_archive_truncation_keeps_newest_and_notes_it() {
        let dir = temp_dump_dir("arch-trunc");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("body-1700000000000.json"), b"a").unwrap();
        std::fs::write(dir.join("body-1700000001000.json"), b"b").unwrap();
        std::fs::write(dir.join("body-1700000002000.json"), b"c").unwrap();

        let files = dump::list_dump_files_in(&dir);
        // 上限口径现含中央目录与 EOCD：单条 = 本地头 53 + 数据 1 + 中央目录 69 + EOCD 22 = 145
        let streamed = collect_archive_with_limit(dir.clone(), files, 150).await;
        let recovered = unzip_stored(&streamed);

        assert_eq!(
            recovered
                .get("body-1700000002000.json")
                .map(|v| v.as_slice()),
            Some(b"c".as_slice()),
            "截断必须保留最新文件"
        );
        assert!(
            !recovered.contains_key("body-1700000001000.json")
                && !recovered.contains_key("body-1700000000000.json"),
            "整文件粒度：超限文件应被整体跳过而非截半"
        );

        let notes = recovered.get(README_ENTRY).expect("截断时应生成说明文件");
        let notes = String::from_utf8_lossy(notes);
        assert!(notes.contains("truncated: true"), "{notes}");
        assert!(notes.contains("body-1700000001000.json"), "{notes}");

        // README 计入归档条目总数：EOCD total entries = 1 数据 + 1 说明
        let eocd_at = streamed.len() - zip::EOCD_LEN;
        let total = u16::from_le_bytes([streamed[eocd_at + 10], streamed[eocd_at + 11]]);
        assert_eq!(total, 2, "说明文件应计入 total entries");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_archive_stream_skips_missing_mid_stream_and_notes_it() {
        let dir = temp_dump_dir("arch-missing");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("body-1700000000000.json"), b"a").unwrap();
        std::fs::write(dir.join("body-1700000001000.json"), b"b").unwrap();

        let files = dump::list_dump_files_in(&dir);
        // 在流开始前删掉其中一个，模拟滚动清理竞态
        std::fs::remove_file(dir.join("body-1700000001000.json")).unwrap();

        let streamed = collect_archive(dir.clone(), files).await;
        let recovered = unzip_stored(&streamed);

        assert!(recovered.contains_key("body-1700000000000.json"));
        assert!(
            !recovered.contains_key("body-1700000001000.json"),
            "已消失的文件不应产生损坏条目"
        );
        let notes = recovered.get(README_ENTRY).expect("应生成说明文件");
        let notes = String::from_utf8_lossy(notes);
        assert!(
            notes.contains("body-1700000001000.json"),
            "说明文件应记录被跳过的文件名"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 归档生成器状态不累积已发送文件的数据字节（数据缓冲 O(1)）。
    ///
    /// 断言状态字段而非仅断言输出：`pending` 严格递减（处理完的条目立即移除）、
    /// `central` 仅按条数累积元数据，持有量与文件数据体积无关。
    #[tokio::test]
    async fn test_generator_state_does_not_accumulate_sent_data() {
        let dir = temp_dump_dir("arch-o1");
        std::fs::create_dir_all(&dir).unwrap();
        let names = [
            "body-1700000000000.json",
            "body-1700000001000.json",
            "body-1700000002000.json",
        ];
        let sizes = [4 * 1024usize, 8 * 1024, 16 * 1024];
        for (name, size) in names.iter().zip(sizes) {
            std::fs::write(dir.join(name), vec![b'x'; size]).unwrap();
        }

        let files = dump::list_dump_files_in(&dir);
        let initial_pending = files.len();
        let mut st = ArchiveState {
            dir: dir.clone(),
            pending: files,
            max_bytes: MAX_ARCHIVE_BYTES,
            offset: 0,
            central_len: 0,
            central: Vec::new(),
            notes: Vec::new(),
            truncated: false,
        };

        // pop() 自尾部取出 → 最新优先，与列表（升序）相反
        let expected: Vec<(&str, usize)> = names
            .iter()
            .rev()
            .zip(sizes.iter().rev())
            .map(|(n, s)| (*n, *s))
            .collect();

        let mut emitted = 0usize;
        while let Some(chunk) = take_next_entry_chunk(&mut st).await {
            let (name, size) = expected[emitted];
            let data = vec![b'x'; size];
            let mut want = zip::local_header(name, zip::entry_crc(&data), size as u32);
            want.extend_from_slice(&data);
            assert_eq!(
                chunk,
                want,
                "第 {} 个 chunk 应只含 {name} 一个文件",
                emitted + 1
            );
            emitted += 1;

            assert_eq!(
                st.pending.len(),
                initial_pending - emitted,
                "已产出条目不得留在 pending 中累积"
            );
            assert_eq!(st.central.len(), emitted, "中央目录每条只累积一次元数据");
        }

        assert_eq!(emitted, 3);
        assert!(st.pending.is_empty());
        assert!(st.notes.is_empty() && !st.truncated);

        // 元数据总额（每条 46B + 文件名）与数据总额无关：28 KiB 数据 vs 约 0.2 KiB 元数据
        let data_total: u64 = sizes.iter().map(|s| *s as u64).sum();
        let meta_total: u64 = names
            .iter()
            .map(|n| (zip::CENTRAL_ENTRY_LEN + n.len()) as u64)
            .sum();
        assert_eq!(
            st.central_len as u64, meta_total,
            "central_len 应恰为各条元数据之和"
        );
        assert!(meta_total * 100 < data_total, "持有量应与数据体积解耦");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_archive_no_notes_when_nothing_skipped() {
        let dir = temp_dump_dir("arch-clean");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("body-1700000000000.json"), b"a").unwrap();

        let files = dump::list_dump_files_in(&dir);
        let streamed = collect_archive(dir.clone(), files).await;
        let recovered = unzip_stored(&streamed);
        assert!(
            !recovered.contains_key(README_ENTRY),
            "未发生截断或跳过时不应生成说明文件"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_archive_entry_count_includes_readme() {
        // 用纯 helper 校验「README 计入 total entries」的算术：2 数据 + 1 说明 = 3
        let archive = zip::build_archive(&[
            ("body-1.json".to_string(), b"a".to_vec()),
            ("body-2.json".to_string(), b"b".to_vec()),
            (README_ENTRY.to_string(), b"note".to_vec()),
        ]);
        let eocd_at = archive.len() - zip::EOCD_LEN;
        let total = u16::from_le_bytes([archive[eocd_at + 10], archive[eocd_at + 11]]);
        assert_eq!(total, 3, "说明文件应计入 total entries");
    }

    /// 最小 STORED ZIP 解析器（测试专用）：按 EOCD → 本地头遍历还原条目。
    /// 独立于实现产出，用于交叉验证归档结构。
    fn unzip_stored(archive: &[u8]) -> std::collections::HashMap<String, Vec<u8>> {
        let mut out = std::collections::HashMap::new();
        let mut pos = 0usize;
        while pos + zip::LOCAL_HEADER_LEN <= archive.len() {
            let sig = u32::from_le_bytes([
                archive[pos],
                archive[pos + 1],
                archive[pos + 2],
                archive[pos + 3],
            ]);
            if sig != 0x0403_4b50 {
                break;
            }
            let name_len = u16::from_le_bytes([archive[pos + 26], archive[pos + 27]]) as usize;
            let size = u32::from_le_bytes([
                archive[pos + 18],
                archive[pos + 19],
                archive[pos + 20],
                archive[pos + 21],
            ]) as usize;
            let crc = u32::from_le_bytes([
                archive[pos + 14],
                archive[pos + 15],
                archive[pos + 16],
                archive[pos + 17],
            ]);
            let name =
                String::from_utf8_lossy(&archive[pos + 30..pos + 30 + name_len]).into_owned();
            let data_at = pos + zip::LOCAL_HEADER_LEN + name_len;
            let data = archive[data_at..data_at + size].to_vec();
            // 顺带校验 CRC 与数据自洽
            assert_eq!(crc, zip::entry_crc(&data), "{name} 的 CRC 与数据不符");
            out.insert(name, data);
            pos = data_at + size;
        }
        out
    }
}
