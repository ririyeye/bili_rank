//! B站数据爬虫模块

use crate::state::{format_online_count, parse_online_total_string, RankingEntry, SharedState};
use crate::Args;
use futures::stream::{self, StreamExt};
use reqwest::Client;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::fs;
use tokio::time::sleep;
use tracing::{error, info, warn};

const BILIBILI_API_HOST: &str = "https://api.bilibili.com";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
const COOKIE: &str = "buvid3=2D4B09A5-0E5F-4537-9F7C-E293CE7324F7167646infoc";

/// 创建 HTTP 客户端
fn create_client() -> reqwest::Result<Client> {
    Client::builder()
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(10))
        .user_agent(USER_AGENT)
        .build()
}

/// 获取排行榜列表
async fn fetch_ranking(client: &Client) -> Option<Vec<RankingEntry>> {
    let url = format!(
        "{}/x/web-interface/ranking/v2?rid=0&type=all",
        BILIBILI_API_HOST
    );

    let response = client
        .get(&url)
        .header("Referer", "https://www.bilibili.com/v/popular/rank/all")
        .header("Origin", "https://www.bilibili.com")
        .header("Cookie", COOKIE)
        .header("Accept", "application/json")
        .header("Accept-Language", "zh-CN,zh;q=0.9")
        .send()
        .await
        .ok()?;

    if !response.status().is_success() {
        warn!("[bili] ranking api returned status {}", response.status());
        return None;
    }

    let json: Value = response.json().await.ok()?;

    // 检查返回码
    if json.get("code").and_then(|v| v.as_i64()) != Some(0) {
        warn!("[bili] ranking api returned unexpected code");
        return None;
    }

    let list = json
        .get("data")
        .and_then(|d| d.get("list"))
        .and_then(|l| l.as_array())?;

    let mut entries = Vec::new();
    for item in list {
        let bvid = item.get("bvid").and_then(|v| v.as_str()).unwrap_or("");
        if bvid.is_empty() {
            continue;
        }

        let cid = item.get("cid").and_then(|v| v.as_i64()).unwrap_or(0);
        if cid == 0 {
            continue;
        }

        let title = item
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let owner = item.get("owner");
        let owner_name = owner
            .and_then(|o| o.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let owner_mid = owner
            .and_then(|o| o.get("mid"))
            .map(|v| {
                if let Some(s) = v.as_str() {
                    s.to_string()
                } else if let Some(n) = v.as_i64() {
                    n.to_string()
                } else {
                    String::new()
                }
            })
            .unwrap_or_default();

        let pic = item
            .get("pic")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        entries.push(RankingEntry {
            bvid: bvid.to_string(),
            title,
            owner_name,
            owner_mid,
            pic,
            cid,
            online_total: 0,
        });
    }

    Some(entries)
}

/// 并发获取所有视频的在线人数
async fn fetch_online_counts(
    client: &Client,
    entries: &mut [RankingEntry],
    state: &Arc<SharedState>,
    concurrency: usize,
    log_error_json: bool,
) {
    let total = entries.len();
    if total == 0 {
        state.finish_fetching(0);
        return;
    }

    let completed = Arc::new(AtomicUsize::new(0));

    // 将 entries 转换为带索引的 Vec，用于并发处理
    let entry_data: Vec<_> = entries
        .iter()
        .map(|e| (e.bvid.clone(), e.cid))
        .collect();

    // 存储结果
    let results: Arc<parking_lot::Mutex<HashMap<String, i64>>> =
        Arc::new(parking_lot::Mutex::new(HashMap::new()));

    // 创建任务
    let tasks = entry_data.into_iter().map(|(bvid, cid)| {
        let client = client.clone();
        let completed = Arc::clone(&completed);
        let state = Arc::clone(state);
        let results = Arc::clone(&results);
        async move {
            if state.should_stop() {
                return;
            }

            // 获取在线人数
            let online_total = fetch_online_count_simple(&client, &bvid, cid, log_error_json).await;

            // 存储结果
            results.lock().insert(bvid, online_total);

            // 更新进度
            let done = completed.fetch_add(1, Ordering::AcqRel) + 1;
            state.update_progress(done);

            if total > 0 {
                let percent = (done as f64 / total as f64) * 100.0;
                info!("[bili] progress {:.0}% ({}/{})", percent, done, total);
            }
        }
    });

    // 使用 buffer_unordered 进行并发控制
    stream::iter(tasks)
        .buffer_unordered(concurrency)
        .collect::<Vec<()>>()
        .await;

    // 将结果写回 entries
    let results_map = results.lock();
    for entry in entries.iter_mut() {
        if let Some(&count) = results_map.get(&entry.bvid) {
            entry.online_total = count;
        }
    }

    let final_completed = completed.load(Ordering::Acquire);
    state.finish_fetching(final_completed);
}

/// 简化版获取在线人数（返回值而不是修改引用）
async fn fetch_online_count_simple(
    client: &Client,
    bvid: &str,
    cid: i64,
    log_error_json: bool,
) -> i64 {
    let url = format!(
        "{}/x/player/online/total?bvid={}&cid={}",
        BILIBILI_API_HOST, bvid, cid
    );

    let referer = format!("https://www.bilibili.com/video/{}", bvid);

    let response = match client
        .get(&url)
        .header("Referer", &referer)
        .header("Origin", "https://www.bilibili.com")
        .header("Cookie", COOKIE)
        .header("Accept", "application/json")
        .header("Accept-Language", "zh-CN,zh;q=0.9")
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!("[bili] online api request failed for {}: {}", bvid, e);
            return 0;
        }
    };

    if !response.status().is_success() {
        warn!(
            "[bili] online api returned status {} for {}",
            response.status(),
            bvid
        );
        return 0;
    }

    let json: Value = match response.json().await {
        Ok(j) => j,
        Err(e) => {
            warn!("[bili] online api json parse failed for {}: {}", bvid, e);
            return 0;
        }
    };

    if json.get("code").and_then(|v| v.as_i64()) != Some(0) {
        if log_error_json {
            warn!(
                "[bili] online api returned non-zero code for {}: {}",
                bvid, json
            );
        } else {
            warn!(
                "[bili] online api returned non-zero code for {} (use --log-error-json to dump)",
                bvid
            );
        }
        return 0;
    }

    // 解析在线人数
    let total = json.get("data").and_then(|d| d.get("total"));
    match total {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        Some(Value::String(s)) => parse_online_total_string(s).unwrap_or_else(|| {
            warn!("[bili] online total parse failed for {}: {}", bvid, s);
            0
        }),
        _ => {
            warn!("[bili] unexpected total type for {}", bvid);
            0
        }
    }
}

/// 构建结果 JSON
fn build_result_payload(entries: &[RankingEntry]) -> Value {
    let mut result = serde_json::Map::new();

    for entry in entries {
        let node = json!({
            "title": entry.title,
            "owner": entry.owner_name,
            "mid": entry.owner_mid,
            "pic": entry.pic,
            "online_count": format_online_count(entry.online_total),
            "count_num": entry.online_total,
        });
        result.insert(entry.bvid.clone(), node);
    }

    Value::Object(result)
}

/// 运行轮询任务
pub async fn run_polling_task(state: Arc<SharedState>, args: Args) {
    let client = match create_client() {
        Ok(c) => c,
        Err(e) => {
            error!("[bili] Failed to create HTTP client: {}", e);
            return;
        }
    };

    let interval = Duration::from_secs(args.interval);

    while !state.should_stop() {
        info!("[bili] Starting ranking fetch...");

        match fetch_ranking(&client).await {
            Some(mut entries) => {
                let count = entries.len();
                info!("[bili] Fetched {} ranking entries", count);

                // 设置抓取状态
                state.set_fetching(true, count);

                // 根据是否是首次抓取选择并发数
                let concurrency = if state.is_initial_fetch_done() {
                    args.normal_concurrency
                } else {
                    args.rapid_concurrency
                };

                info!("[bili] Using concurrency: {}", concurrency);

                // 获取在线人数
                fetch_online_counts(
                    &client,
                    &mut entries,
                    &state,
                    concurrency,
                    args.log_error_json,
                )
                .await;

                if !entries.is_empty() {
                    // 构建并保存结果
                    let payload = build_result_payload(&entries);
                    let serialized = serde_json::to_string_pretty(&payload).unwrap_or_default();

                    // 更新状态
                    state.update_payload(serialized.clone());

                    // 如果需要，写入文件
                    if args.output_file {
                        if let Err(e) = fs::write(&args.output_path, &serialized).await {
                            warn!("[bili] Failed to write {}: {}", args.output_path, e);
                        } else {
                            info!("[bili] Written to {}", args.output_path);
                        }
                    }

                    info!("[bili] Ranking updated, {} entries", count);
                } else {
                    state.set_initial_fetch_done();
                }
            }
            None => {
                warn!("[bili] Failed to fetch ranking");
                state.finish_fetching(0);
            }
        }

        // 检查是否应该停止
        if state.should_stop() {
            break;
        }

        // 等待下一轮
        info!("[bili] Waiting {}s for next fetch...", args.interval);

        // 分段等待，以便及时响应停止信号
        let check_interval = Duration::from_secs(1);
        let mut remaining = interval;
        while remaining > Duration::ZERO && !state.should_stop() {
            let wait_time = remaining.min(check_interval);
            sleep(wait_time).await;
            remaining = remaining.saturating_sub(wait_time);
        }
    }

    info!("[bili] Polling task stopped");
}
