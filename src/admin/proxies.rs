//! 管理员代理池 API；响应只包含安全摘要，不回显导入行或认证信息。
use axum::{
    Json,
    extract::{
        Path, State,
        rejection::{JsonRejection, PathRejection},
    },
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

use super::middleware::AdminState;
use crate::model::proxy_pool::{ProxyPool, ProxySummary};

#[derive(Deserialize)]
pub(super) struct ImportRequest {
    text: String,
}
#[derive(Deserialize)]
pub(super) struct EnabledRequest {
    enabled: bool,
}

fn error(status: StatusCode, message: &'static str) -> Response {
    (
        status,
        Json(json!({"error":{"type":"proxy_pool_error","message":message}})),
    )
        .into_response()
}

#[allow(clippy::result_large_err)] // Axum 响应边界直接交还既定错误响应，不跨模块传播。
fn pool(state: &AdminState) -> Result<Arc<ProxyPool>, Response> {
    state.proxy_pool.clone().ok_or_else(|| {
        error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Proxy pool is unavailable on this server.",
        )
    })
}

#[allow(clippy::result_large_err)] // 同一处理器中的响应提取边界，保留固定脱敏响应。
fn proxy_id(path: Result<Path<u64>, PathRejection>) -> Result<u64, Response> {
    path.map(|Path(id)| id)
        .map_err(|_| error(StatusCode::BAD_REQUEST, "Invalid proxy identifier."))
}

fn enrich(state: &AdminState, summaries: &mut [ProxySummary]) {
    let bindings = state.service.proxy_bindings();
    for summary in summaries {
        summary.assigned_account_ids = bindings.get(&summary.id).cloned().unwrap_or_default();
        summary.assigned_account_ids.sort_unstable();
        summary.assigned_accounts = summary.assigned_account_ids.len();
    }
}

pub(super) async fn list_proxies(State(state): State<AdminState>) -> Response {
    let Some(pool) = &state.proxy_pool else {
        return Json(json!({"proxies":[],"configured":false})).into_response();
    };
    let mut summaries = pool.summaries();
    enrich(&state, &mut summaries);
    Json(json!({"proxies":summaries,"configured":pool.is_configured()})).into_response()
}

pub(super) async fn import_proxies(
    State(state): State<AdminState>,
    request: Result<Json<ImportRequest>, JsonRejection>,
) -> Response {
    let request = match request {
        Ok(Json(request)) => request,
        Err(_) => return error(StatusCode::BAD_REQUEST, "Invalid proxy import request."),
    };
    let pool = match pool(&state) {
        Ok(pool) => pool,
        Err(error) => return error,
    };
    let importing = pool.clone();
    let result = tokio::task::spawn_blocking(move || importing.import(&request.text)).await;
    match result {
        Ok(Ok(result)) => {
            if result.imported > 0 {
                tokio::spawn(async move {
                    pool.check_due().await;
                });
            }
            Json(result).into_response()
        }
        Ok(Err(_)) => error(
            StatusCode::BAD_REQUEST,
            "Proxy import could not be saved or exceeds the size limit.",
        ),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Proxy import could not finish.",
        ),
    }
}

pub(super) async fn check_all_proxies(State(state): State<AdminState>) -> Response {
    let pool = match pool(&state) {
        Ok(pool) => pool,
        Err(error) => return error,
    };
    tokio::spawn(async move {
        pool.check_all().await;
    });
    (StatusCode::ACCEPTED, Json(json!({"scheduled":true}))).into_response()
}

pub(super) async fn check_proxy(
    State(state): State<AdminState>,
    path: Result<Path<u64>, PathRejection>,
) -> Response {
    let id = match proxy_id(path) {
        Ok(id) => id,
        Err(error) => return error,
    };
    let pool = match pool(&state) {
        Ok(pool) => pool,
        Err(error) => return error,
    };
    match pool.summary(id) {
        None => return error(StatusCode::NOT_FOUND, "Proxy was not found."),
        Some(summary) if !summary.enabled => {
            return error(
                StatusCode::BAD_REQUEST,
                "Enable the proxy before checking it.",
            );
        }
        _ => {}
    }
    tokio::spawn(async move {
        let _ = pool.check(id).await;
    });
    (StatusCode::ACCEPTED, Json(json!({"scheduled":true}))).into_response()
}

pub(super) async fn set_proxy_enabled(
    State(state): State<AdminState>,
    path: Result<Path<u64>, PathRejection>,
    request: Result<Json<EnabledRequest>, JsonRejection>,
) -> Response {
    let id = match proxy_id(path) {
        Ok(id) => id,
        Err(error) => return error,
    };
    let request = match request {
        Ok(Json(request)) => request,
        Err(_) => return error(StatusCode::BAD_REQUEST, "Invalid proxy update request."),
    };
    let pool = match pool(&state) {
        Ok(pool) => pool,
        Err(error) => return error,
    };
    if pool.summary(id).is_none() {
        return error(StatusCode::NOT_FOUND, "Proxy was not found.");
    }
    let updating = pool.clone();
    match tokio::task::spawn_blocking(move || updating.set_enabled(id, request.enabled)).await {
        Ok(Ok(mut summary)) => {
            enrich(&state, std::slice::from_mut(&mut summary));
            if summary.enabled {
                tokio::spawn(async move {
                    let _ = pool.check(id).await;
                });
            }
            Json(json!({"proxy":summary})).into_response()
        }
        _ => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Proxy update could not be saved.",
        ),
    }
}

pub(super) async fn delete_proxy(
    State(state): State<AdminState>,
    path: Result<Path<u64>, PathRejection>,
) -> Response {
    let id = match proxy_id(path) {
        Ok(id) => id,
        Err(error) => return error,
    };
    let pool = match pool(&state) {
        Ok(pool) => pool,
        Err(error) => return error,
    };
    if pool.summary(id).is_none() {
        return error(StatusCode::NOT_FOUND, "Proxy was not found.");
    }
    match tokio::task::spawn_blocking(move || pool.delete(id)).await {
        Ok(Ok(())) => Json(json!({"deleted":true})).into_response(),
        _ => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Proxy deletion could not be saved.",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::{AdminService, create_admin_router};
    use crate::kiro::token_manager::MultiTokenManager;
    use crate::model::config::Config;

    struct Fixture {
        directory: std::path::PathBuf,
        task: tokio::task::JoinHandle<()>,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.task.abort();
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    #[tokio::test]
    async fn proxy_admin_routes_require_auth_and_all_validation_errors_are_redacted() {
        let directory =
            std::env::temp_dir().join(format!("kiro-proxy-api-{}", uuid::Uuid::new_v4()));
        let pool =
            Arc::new(ProxyPool::load(directory.join("proxies.json"), &Config::default()).unwrap());
        const INPUT: &str = "gateway.example:10001:demo-user:demo~password:colon";
        pool.import(INPUT).unwrap();
        let manager =
            Arc::new(MultiTokenManager::new(Config::default(), vec![], None, None, true).unwrap());
        let state = AdminState::new(
            Arc::new(parking_lot::RwLock::new("demo-admin".to_string())),
            AdminService::new(manager),
        )
        .with_proxy_pool(pool.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/api/admin", listener.local_addr().unwrap());
        let app = axum::Router::new().nest("/api/admin", create_admin_router(state));
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let _fixture = Fixture { directory, task };
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        for (method, path) in [
            (reqwest::Method::GET, "/proxies"),
            (reqwest::Method::POST, "/proxies/import"),
            (reqwest::Method::POST, "/proxies/check"),
            (reqwest::Method::POST, "/proxies/1/check"),
            (reqwest::Method::PUT, "/proxies/1/enabled"),
            (reqwest::Method::DELETE, "/proxies/1"),
        ] {
            let response = client
                .request(method, format!("{url}{path}"))
                .json(&json!({"text":INPUT,"enabled":false}))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
        let response = client
            .get(format!("{url}/proxies"))
            .header("x-api-key", "demo-admin")
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        let summary = response.text().await.unwrap();
        assert!(!summary.contains("demo-user"));
        assert!(!summary.contains("demo~password:colon"));
        assert!(!summary.contains("@gateway"));
        for (method, path, body) in [
            (
                reqwest::Method::POST,
                "/proxies/import".to_string(),
                json!({"text":[INPUT]}),
            ),
            (
                reqwest::Method::PUT,
                "/proxies/1/enabled".to_string(),
                json!({"enabled":INPUT}),
            ),
            (
                reqwest::Method::DELETE,
                format!("/proxies/{}", urlencoding::encode(INPUT)),
                json!({}),
            ),
        ] {
            let response = client
                .request(method, format!("{url}{path}"))
                .header("x-api-key", "demo-admin")
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let error = response.text().await.unwrap();
            assert!(!error.contains("demo-user"));
            assert!(!error.contains("demo~password:colon"));
        }
        let disabled = client
            .put(format!("{url}/proxies/1/enabled"))
            .header("x-api-key", "demo-admin")
            .json(&json!({"enabled":false}))
            .send()
            .await
            .unwrap();
        assert!(disabled.status().is_success());
        assert!(!pool.summary(1).unwrap().enabled);
        let deleted = client
            .delete(format!("{url}/proxies/1"))
            .header("x-api-key", "demo-admin")
            .send()
            .await
            .unwrap();
        assert!(deleted.status().is_success());
        assert!(pool.is_configured());
        assert!(pool.summaries().is_empty());
    }
}
