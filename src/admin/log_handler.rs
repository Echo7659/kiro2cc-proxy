// Copyright (c) 2026 Harllan He. Licensed under MIT.
//! 日志查看 API Handler — SSE 流 + 下载接口

use std::collections::HashMap;
use std::convert::Infallible;
use std::time::Duration;

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
};
use futures::stream::{self, Stream, StreamExt};
use tokio::sync::broadcast::{Receiver, error::RecvError};

use super::middleware::AdminState;
use crate::common::auth;
use crate::log_capture::LogEntry;

/// 日志页首屏（快照 / SSE history）默认只取最新这么多条。
/// ring buffer 容量更大（用于完整下载），首屏不必全量传输。
const VIEW_DEFAULT_LIMIT: usize = 2000;

/// 解析 `limit` 查询参数：缺省或非法值取 `VIEW_DEFAULT_LIMIT`；超过缓冲区现有条数时自然返回全部
fn parse_limit(params: &HashMap<String, String>) -> usize {
    params
        .get("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(VIEW_DEFAULT_LIMIT)
}

/// GET /api/admin/logs/stream?api_key=<key>
///
/// EventSource 不支持自定义 Header，因此通过 Query Param 认证。
/// 连接后先发送 history 事件（最新 `limit` 条，默认 2000），随后持续推送 log 事件。
///
/// # 安全说明
/// API Key 以 Query Param 形式传输，会出现在 access log 中。
/// 生产环境部署时必须在 HTTPS 反向代理后方使用本端点。
pub async fn stream_logs(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if !check_api_key(&state, &headers, &params) {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    let Some(log_capture) = &state.log_capture else {
        return (StatusCode::SERVICE_UNAVAILABLE, "Log capture not enabled").into_response();
    };
    let log_capture = log_capture.clone();

    // 先订阅，再取快照，避免遗漏订阅后、快照前的新事件
    let rx = log_capture.subscribe();
    let history = log_capture.snapshot_tail(parse_limit(&params));
    let history_json = serde_json::to_string(&history).unwrap_or_else(|_| "[]".to_string());

    let history_stream = stream::once(async move {
        Ok::<Event, Infallible>(Event::default().event("history").data(history_json))
    });

    let mut response = Sse::new(history_stream.chain(live_log_stream(rx)))
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("ping"),
        )
        .into_response();

    // 禁止反向代理（Nginx/Caddy/Cloudflare）缓冲 SSE 响应体
    let headers = response.headers_mut();
    headers.insert("X-Accel-Buffering", "no".parse().unwrap());
    headers.insert("X-Content-Type-Options", "nosniff".parse().unwrap());

    response
}

/// 日志通道的消费者不能通过 tracing 报告自身滞后，否则告警重新广播后会继续挤掉
/// 本接收者的最旧未读条目，形成无需外部生产者也能持续的 Lagged(1) 自激环。
/// 滞后仅作为客户端事件发送，下一次轮询从广播通道的最旧留存条目继续。
fn live_log_stream(rx: Receiver<LogEntry>) -> impl Stream<Item = Result<Event, Infallible>> {
    stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(entry) => {
                    let Ok(json) = serde_json::to_string(&entry) else {
                        continue;
                    };
                    return Some((Ok(Event::default().event("log").data(json)), rx));
                }
                Err(RecvError::Lagged(n)) => {
                    return Some((
                        Ok(Event::default()
                            .event("lagged")
                            .data(format!("{{\"dropped\":{n}}}"))),
                        rx,
                    ));
                }
                Err(RecvError::Closed) => return None,
            }
        }
    })
}

/// GET /api/admin/logs/snapshot?api_key=<key>
///
/// 返回 ring buffer 中最新 `limit` 条日志的 JSON 数组（与 SSE history 事件格式一致）。
/// `limit` 缺省为 2000；日志页「下载」传大值（如 100000）取回完整缓冲区。
/// 前端在 SSE 连接建立后立即调用此接口获取初始数据，规避代理对 SSE body 的缓冲。
///
/// 除 `api_key` 查询参数外，也接受 `x-api-key` / `Authorization: Bearer` 头，
/// 使下载这类一次性请求不必把密钥放进 URL。
pub async fn snapshot_logs(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if !check_api_key(&state, &headers, &params) {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    let Some(log_capture) = &state.log_capture else {
        return (StatusCode::SERVICE_UNAVAILABLE, "Log capture not enabled").into_response();
    };

    let snapshot = log_capture.snapshot_tail(parse_limit(&params));
    let json = serde_json::to_string(&snapshot).unwrap_or_else(|_| "[]".to_string());

    let mut resp_headers = axum::http::HeaderMap::new();
    resp_headers.insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json; charset=utf-8"),
    );
    (resp_headers, json).into_response()
}

/// GET /api/admin/logs/download
///
/// 返回当前 ring buffer 内全部日志（不支持 `limit` / 过滤），以 .txt 文件形式下载。
/// Web UI 的「下载」已改走 `/logs/snapshot` 并在前端按筛选条件组装，本端点保留给脚本 / curl。
///
/// # 安全说明
/// 认证同其他日志端点：`api_key` 查询参数，或 `x-api-key` / `Authorization: Bearer` 头。
/// 用查询参数时密钥会出现在反代 access log 中，必须在 HTTPS 后方使用；脚本优先用头。
pub async fn download_logs(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if !check_api_key(&state, &headers, &params) {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    let Some(log_capture) = &state.log_capture else {
        return (StatusCode::SERVICE_UNAVAILABLE, "Log capture not enabled").into_response();
    };

    let snapshot = log_capture.snapshot();
    let mut content = String::with_capacity(snapshot.len() * 120);
    for entry in &snapshot {
        content.push_str(&format!(
            "{} [{}] {} {}\n",
            entry.timestamp, entry.level, entry.target, entry.message
        ));
    }

    let timestamp = chrono::Utc::now().format("%Y%m%d-%H%M%S").to_string();
    let disposition = format!(
        "attachment; filename=\"kiro2cc-proxy-logs-{}.txt\"",
        timestamp
    );

    let mut resp_headers = axum::http::HeaderMap::new();
    resp_headers.insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    resp_headers.insert(
        header::CONTENT_DISPOSITION,
        axum::http::HeaderValue::from_str(&disposition)
            .unwrap_or_else(|_| axum::http::HeaderValue::from_static("attachment")),
    );

    (resp_headers, content).into_response()
}

/// 认证：`api_key` 查询参数（EventSource 只能用它），或 `x-api-key` / `Bearer` 头（fetch 可用）
fn check_api_key(
    state: &AdminState,
    headers: &HeaderMap,
    params: &HashMap<String, String>,
) -> bool {
    let key = auth::extract_api_key_from_headers(headers)
        .or_else(|| params.get("api_key").map(|s| s.as_str()))
        .unwrap_or("");
    auth::constant_time_eq(key, &state.admin_psw.read())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log_capture::LogCapture;
    use axum::body::BodyDataStream;
    use tracing_subscriber::prelude::*;

    async fn next_frame(body: &mut BodyDataStream) -> String {
        let chunk = tokio::time::timeout(Duration::from_secs(1), body.next())
            .await
            .expect("本地已就绪的日志帧必须及时送出，不能进入自激循环")
            .expect("日志通道仍打开")
            .expect("SSE 序列化应成功");
        String::from_utf8(chunk.to_vec()).unwrap()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn lagged_consumer_recovers_without_generating_more_logs() {
        let capture = LogCapture::new(1000);
        let rx = capture.subscribe();
        let mut other_rx = capture.subscribe();
        let subscriber = tracing_subscriber::registry().with(capture.as_layer());
        // 当前线程持有真实捕获层：若 Lagged 分支重新使用 tracing，会送回被测通道。
        let _subscriber = tracing::subscriber::set_default(subscriber);
        for index in 0..257 {
            tracing::info!("synthetic log entry index={index}");
        }
        assert_eq!(capture.snapshot().len(), 257);
        let mut body = Sse::new(live_log_stream(rx))
            .into_response()
            .into_body()
            .into_data_stream();

        let lagged = next_frame(&mut body).await;
        assert!(lagged.contains("event: lagged"), "{lagged}");
        assert!(lagged.contains("\"dropped\":1"), "{lagged}");
        for index in 1..257 {
            let frame = next_frame(&mut body).await;
            assert!(frame.contains("event: log"), "{frame}");
            let data = frame
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .unwrap();
            let entry: serde_json::Value = serde_json::from_str(data).unwrap();
            assert_eq!(
                entry["message"],
                format!("synthetic log entry index={index}")
            );
        }
        // 没有新生产者时接收恢复为空闲，缓存和其他消费者也没有收到自造的告警。
        assert!(
            tokio::time::timeout(Duration::from_millis(10), body.next())
                .await
                .is_err()
        );
        assert_eq!(capture.snapshot().len(), 257);
        assert!(matches!(
            other_rx.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(1))
        ));
        for _ in 0..256 {
            assert!(other_rx.try_recv().is_ok());
        }
        assert!(matches!(
            other_rx.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));

        // 同一连接之后的新日志仍会送达，不因一次滞后永久中断。
        tracing::info!("synthetic log after recovery");
        assert!(
            next_frame(&mut body)
                .await
                .contains("synthetic log after recovery")
        );
        assert_eq!(capture.snapshot().len(), 258);
    }
}
