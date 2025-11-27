//! B站视频实时在线人数排行榜 - Rust 版本爬虫
//!
//! 功能:
//! - 获取B站排行榜视频列表
//! - 并发获取每个视频的实时在线人数
//! - 提供HTTP服务器访问数据
//! - 第一轮查询快速并发，后续按间隔定时查询

mod crawler;
mod server;
mod state;

use clap::Parser;
use std::sync::Arc;
use tokio::signal;
use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;

use crate::state::SharedState;

/// B站视频实时在线人数排行榜爬虫
#[derive(Parser, Debug, Clone)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// 监听地址
    #[arg(long, default_value = "0.0.0.0")]
    host: String,

    /// 监听端口
    #[arg(long, default_value_t = 8000)]
    port: u16,

    /// 查询间隔(秒)
    #[arg(long, default_value_t = 300)]
    interval: u64,

    /// 是否输出 data.json 文件
    #[arg(long, default_value_t = false)]
    output_file: bool,

    /// 输出文件路径
    #[arg(long, default_value = "data.json")]
    output_path: String,

    /// 第一轮并发数
    #[arg(long, default_value_t = 20)]
    rapid_concurrency: usize,

    /// 后续轮次并发数
    #[arg(long, default_value_t = 5)]
    normal_concurrency: usize,

    /// 是否记录错误响应的JSON
    #[arg(long, default_value_t = false)]
    log_error_json: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 初始化日志
    FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .with_target(false)
        .with_thread_ids(false)
        .compact()
        .init();

    let args = Args::parse();

    info!(
        "[bili] Starting Bilibili ranking crawler on {}:{}",
        args.host, args.port
    );
    info!(
        "[bili] Query interval: {}s, Output file: {}",
        args.interval,
        if args.output_file {
            &args.output_path
        } else {
            "disabled"
        }
    );

    // 创建共享状态
    let state = Arc::new(SharedState::new());

    // 克隆用于不同任务
    let crawler_state = Arc::clone(&state);
    let server_state = Arc::clone(&state);
    let args_clone = args.clone();

    // 启动爬虫任务
    let crawler_handle = tokio::spawn(async move {
        crawler::run_polling_task(crawler_state, args_clone).await;
    });

    // 启动HTTP服务器
    let _server_handle = tokio::spawn(async move {
        if let Err(e) = server::run_server(server_state, &args.host, args.port).await {
            tracing::error!("[bili] Server error: {}", e);
        }
    });

    // 等待 Ctrl+C 信号
    signal::ctrl_c().await?;
    info!("[bili] Received shutdown signal, stopping...");

    // 设置停止标志
    state.set_stop(true);

    // 等待任务结束（带超时）
    tokio::select! {
        _ = crawler_handle => {
            info!("[bili] Crawler task stopped");
        }
        _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
            info!("[bili] Timeout waiting for crawler task");
        }
    }

    info!("[bili] Shutdown complete");
    Ok(())
}
