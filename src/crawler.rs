//! B站数据爬虫模块
//!
//! 接口对齐 bilibili-API-collect：
//! - 排行榜: `/x/web-interface/ranking/v2`（rid/type/web_location + WBI）
//! - 在线人数: `/x/player/online/total`（aid|bvid + cid）

use crate::state::{format_online_count, parse_online_total_string, RankingEntry, SharedState};
use crate::wbi::{encode_wbi, get_wbi_keys};
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
/// BAC ranking.md: web_location = 333.934
const RANKING_WEB_LOCATION: &str = "333.934";

/// 创建 HTTP 客户端
fn create_client() -> reqwest::Result<Client> {
    Client::builder()
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(10))
        .user_agent(USER_AGENT)
        .build()
}

/// 获取排行榜列表（BAC: /x/web-interface/ranking/v2）
async fn fetch_ranking(client: &Client) -> Option<Vec<RankingEntry>> {
    let keys = get_wbi_keys(client, COOKIE).await;
    let params = vec![
        ("rid", "0".to_string()),
        ("type", "all".to_string()),
        ("web_location", RANKING_WEB_LOCATION.to_string()),
    ];

    let url = if let Some(keys) = keys {
        let query = encode_wbi(params, keys);
        format!("{BILIBILI_API_HOST}/x/web-interface/ranking/v2?{query}")
    } else {
        warn!("[bili] WBI keys unavailable, fallback without signature");
        format!(
            "{BILIBILI_API_HOST}/x/web-interface/ranking/v2?rid=0&type=all&web_location={RANKING_WEB_LOCATION}"
        )
    };

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

    if json.get("code").and_then(|v| v.as_i64()) != Some(0) {
        warn!(
            "[bili] ranking api returned unexpected code: {:?}",
            json.get("code")
        );
        return None;
    }

    // 无 WBI 时可能只返回 v_voucher
    if json.get("data").and_then(|d| d.get("v_voucher")).is_some()
        && json.get("data").and_then(|d| d.get("list")).is_none()
    {
        warn!("[bili] ranking api returned v_voucher (WBI required)");
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

        let aid = item.get("aid").and_then(|v| v.as_i64()).unwrap_or(0);
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
            aid,
            bvid: bvid.to_string(),
            title,
            owner_name,
            owner_mid,
            pic,
            cid,
            online_total: 0,
            online_total_text: String::new(),
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

    let entry_data: Vec<_> = entries
        .iter()
        .map(|e| (e.aid, e.bvid.clone(), e.cid))
        .collect();

    let results: Arc<parking_lot::Mutex<HashMap<String, (i64, String)>>> =
        Arc::new(parking_lot::Mutex::new(HashMap::new()));

    let tasks = entry_data.into_iter().map(|(aid, bvid, cid)| {
        let client = client.clone();
        let completed = Arc::clone(&completed);
        let state = Arc::clone(state);
        let results = Arc::clone(&results);
        async move {
            if state.should_stop() {
                return;
            }

            let (online_total, online_text) =
                fetch_online_count(&client, aid, &bvid, cid, log_error_json).await;

            results.lock().insert(bvid, (online_total, online_text));

            let done = completed.fetch_add(1, Ordering::AcqRel) + 1;
            state.update_progress(done);

            if total > 0 {
                let percent = (done as f64 / total as f64) * 100.0;
                info!("[bili] progress {:.0}% ({}/{})", percent, done, total);
            }
        }
    });

    stream::iter(tasks)
        .buffer_unordered(concurrency)
        .collect::<Vec<()>>()
        .await;

    let results_map = results.lock();
    for entry in entries.iter_mut() {
        if let Some((count, text)) = results_map.get(&entry.bvid) {
            entry.online_total = *count;
            entry.online_total_text = text.clone();
        }
    }

    let final_completed = completed.load(Ordering::Acquire);
    state.finish_fetching(final_completed);
}

/// 获取视频在线人数（BAC: /x/player/online/total，优先 aid+cid）
async fn fetch_online_count(
    client: &Client,
    aid: i64,
    bvid: &str,
    cid: i64,
    log_error_json: bool,
) -> (i64, String) {
    // BAC 示例用 aid+cid；无 aid 时回退 bvid+cid
    let url = if aid > 0 {
        format!("{BILIBILI_API_HOST}/x/player/online/total?aid={aid}&cid={cid}")
    } else {
        format!("{BILIBILI_API_HOST}/x/player/online/total?bvid={bvid}&cid={cid}")
    };

    let referer = if !bvid.is_empty() {
        format!("https://www.bilibili.com/video/{bvid}")
    } else {
        format!("https://www.bilibili.com/video/av{aid}")
    };

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
            return (0, "0".to_string());
        }
    };

    if !response.status().is_success() {
        warn!(
            "[bili] online api returned status {} for {}",
            response.status(),
            bvid
        );
        return (0, "0".to_string());
    }

    let json: Value = match response.json().await {
        Ok(j) => j,
        Err(e) => {
            warn!("[bili] online api json parse failed for {}: {}", bvid, e);
            return (0, "0".to_string());
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
        return (0, "0".to_string());
    }

    // BAC: data.total = 所有终端总计人数（字符串，如 `9.4万+`）
    // data.count = web 端实时在线人数（可作数值回退）
    let data = json.get("data");
    let total = data.and_then(|d| d.get("total"));
    let (count_num, total_text) = match total {
        Some(Value::String(s)) => {
            let n = parse_online_total_string(s).unwrap_or_else(|| {
                warn!("[bili] online total parse failed for {}: {}", bvid, s);
                0
            });
            (n, s.clone())
        }
        Some(Value::Number(n)) => {
            let n = n.as_i64().unwrap_or(0);
            (n, format_online_count(n))
        }
        _ => {
            // 回退用 data.count
            match data.and_then(|d| d.get("count")) {
                Some(Value::String(s)) => {
                    let n = parse_online_total_string(s).unwrap_or(0);
                    (n, s.clone())
                }
                Some(Value::Number(n)) => {
                    let n = n.as_i64().unwrap_or(0);
                    (n, n.to_string())
                }
                _ => {
                    warn!("[bili] unexpected total type for {}", bvid);
                    (0, "0".to_string())
                }
            }
        }
    };

    (count_num, total_text)
}

/// 构建结果 JSON（按在线人数排序，取前 top_n 个）
fn build_result_payload(entries: &[RankingEntry], top_n: usize) -> Value {
    let mut sorted_entries: Vec<_> = entries.iter().collect();
    sorted_entries.sort_by(|a, b| b.online_total.cmp(&a.online_total));

    let top_entries: Vec<_> = sorted_entries.into_iter().take(top_n).collect();

    let mut result = serde_json::Map::new();

    for entry in top_entries {
        let online_count = if entry.online_total_text.is_empty() {
            format_online_count(entry.online_total)
        } else {
            entry.online_total_text.clone()
        };

        let node = json!({
            "aid": entry.aid,
            "title": entry.title,
            "owner": entry.owner_name,
            "mid": entry.owner_mid,
            "pic": entry.pic,
            "online_count": online_count,
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
            Some(new_entries) => {
                let new_count = new_entries.len();
                info!("[bili] Fetched {} new ranking entries", new_count);

                let mut merged_entries = state.push_ranking_and_merge(new_entries);
                let merged_count = merged_entries.len();
                let history_count = state.get_history_count();

                info!(
                    "[bili] Merged {} entries from {} history cycles",
                    merged_count, history_count
                );

                state.set_fetching(true, merged_count);

                let concurrency = if state.is_initial_fetch_done() {
                    args.normal_concurrency
                } else {
                    args.rapid_concurrency
                };

                info!("[bili] Using concurrency: {}", concurrency);

                fetch_online_counts(
                    &client,
                    &mut merged_entries,
                    &state,
                    concurrency,
                    args.log_error_json,
                )
                .await;

                if !merged_entries.is_empty() {
                    let payload = build_result_payload(&merged_entries, args.top_n);
                    let serialized = serde_json::to_string_pretty(&payload).unwrap_or_default();

                    state.update_payload(serialized.clone());

                    if args.output_file {
                        if let Err(e) = fs::write(&args.output_path, &serialized).await {
                            warn!("[bili] Failed to write {}: {}", args.output_path, e);
                        } else {
                            info!("[bili] Written to {}", args.output_path);
                        }
                    }

                    info!(
                        "[bili] Ranking updated, top {} of {} entries",
                        args.top_n.min(merged_count),
                        merged_count
                    );
                } else {
                    state.set_initial_fetch_done();
                }
            }
            None => {
                warn!("[bili] Failed to fetch ranking");
                state.finish_fetching(0);
            }
        }

        if state.should_stop() {
            break;
        }

        info!("[bili] Waiting {}s for next fetch...", args.interval);

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
