//! HTTP 服务器模块

use crate::state::SharedState;
use axum::{
    extract::State,
    http::{header, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::get,
    Json, Router,
};
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};
use tracing::info;

/// 内嵌的 index.html 内容
const INDEX_HTML: &str = include_str!("../bili_index.html");

type AppState = Arc<SharedState>;

/// 启动 HTTP 服务器
pub async fn run_server(
    state: AppState,
    host: &str,
    port: u16,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        .route("/", get(index_handler))
        .route("/index.html", get(index_handler))
        .route("/data.json", get(data_handler))
        .route("/progress", get(progress_handler))
        .route("/progress.json", get(progress_handler))
        .route("/status", get(progress_handler))
        .layer(cors)
        .with_state(state);

    let addr = format!("{}:{}", host, port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;

    info!("[bili] HTTP server listening on http://{}", addr);

    axum::serve(listener, app).await?;

    Ok(())
}

/// 首页处理
async fn index_handler() -> impl IntoResponse {
    Html(INDEX_HTML)
}

/// 数据接口处理
async fn data_handler(State(state): State<AppState>) -> Response {
    let (payload, last_modified) = state.get_payload();

    if payload.is_empty() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [
                (header::CONTENT_TYPE, "application/json; charset=utf-8"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            r#"{"error":"data not ready"}"#,
        )
            .into_response();
    }

    let mut response = (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/json; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        payload,
    )
        .into_response();

    if !last_modified.is_empty() {
        response.headers_mut().insert(
            header::LAST_MODIFIED,
            last_modified
                .parse()
                .unwrap_or_else(|_| "".parse().unwrap()),
        );
    }

    response
}

/// 进度接口处理
async fn progress_handler(State(state): State<AppState>) -> impl IntoResponse {
    let progress = state.get_progress();
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/json; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        Json(progress),
    )
}
