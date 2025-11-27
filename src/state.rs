//! 共享状态管理

use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};

/// 排行榜条目
#[derive(Debug, Clone)]
pub struct RankingEntry {
    pub bvid: String,
    pub title: String,
    pub owner_name: String,
    pub owner_mid: String,
    pub pic: String,
    pub cid: i64,
    pub online_total: i64,
}

/// 共享状态
pub struct SharedState {
    inner: RwLock<SharedStateInner>,
    stop: AtomicBool,
}

struct SharedStateInner {
    pub json_payload: String,
    pub last_modified: String,
    pub updated_at: Option<DateTime<Utc>>,
    pub progress_updated: Option<DateTime<Utc>>,
    pub total_items: usize,
    pub completed_items: usize,
    pub fetching: bool,
    pub initial_fetch_done: bool,
}

impl SharedState {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(SharedStateInner {
                json_payload: String::new(),
                last_modified: String::new(),
                updated_at: None,
                progress_updated: None,
                total_items: 0,
                completed_items: 0,
                fetching: false,
                initial_fetch_done: false,
            }),
            stop: AtomicBool::new(false),
        }
    }

    pub fn set_stop(&self, stop: bool) {
        self.stop.store(stop, Ordering::Release);
    }

    pub fn should_stop(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    pub fn is_initial_fetch_done(&self) -> bool {
        self.inner.read().initial_fetch_done
    }

    pub fn set_fetching(&self, fetching: bool, total: usize) {
        let mut inner = self.inner.write();
        inner.fetching = fetching;
        inner.total_items = total;
        inner.completed_items = 0;
        inner.progress_updated = Some(Utc::now());
    }

    pub fn update_progress(&self, completed: usize) {
        let mut inner = self.inner.write();
        inner.completed_items = inner.completed_items.max(completed);
        inner.progress_updated = Some(Utc::now());
    }

    pub fn finish_fetching(&self, completed: usize) {
        let mut inner = self.inner.write();
        inner.fetching = false;
        inner.completed_items = completed.min(inner.total_items);
        inner.progress_updated = Some(Utc::now());
    }

    pub fn set_initial_fetch_done(&self) {
        let mut inner = self.inner.write();
        inner.initial_fetch_done = true;
    }

    pub fn update_payload(&self, payload: String) {
        let now = Utc::now();
        let last_modified = format_http_date(now);
        let mut inner = self.inner.write();
        inner.json_payload = payload;
        inner.last_modified = last_modified;
        inner.updated_at = Some(now);
        inner.initial_fetch_done = true;
    }

    pub fn get_payload(&self) -> (String, String) {
        let inner = self.inner.read();
        (inner.json_payload.clone(), inner.last_modified.clone())
    }

    pub fn get_progress(&self) -> ProgressInfo {
        let inner = self.inner.read();
        let percent = if inner.total_items > 0 {
            (inner.completed_items as f64 / inner.total_items as f64) * 100.0
        } else {
            0.0
        };
        ProgressInfo {
            fetching: inner.fetching,
            completed: inner.completed_items,
            total: inner.total_items,
            percent,
            last_data_time: inner.last_modified.clone(),
            progress_time: inner
                .progress_updated
                .map(format_http_date)
                .unwrap_or_default(),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ProgressInfo {
    pub fetching: bool,
    pub completed: usize,
    pub total: usize,
    pub percent: f64,
    pub last_data_time: String,
    pub progress_time: String,
}

/// 格式化 HTTP 日期
pub fn format_http_date(dt: DateTime<Utc>) -> String {
    dt.format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

/// 格式化在线人数显示
pub fn format_online_count(total: i64) -> String {
    if total >= 10000 {
        let value = total as f64 / 10000.0;
        if value >= 100.0 {
            format!("{:.0}万+", value)
        } else if value >= 10.0 {
            format!("{:.1}万+", value)
        } else {
            format!("{:.2}万+", value)
        }
    } else if total >= 1000 {
        let base = (total / 1000) * 1000;
        format!("{}+", base)
    } else {
        total.to_string()
    }
}

/// 解析在线人数字符串（支持"万"、"亿"单位）
pub fn parse_online_total_string(value: &str) -> Option<i64> {
    let mut filtered = String::new();
    let mut has_wan = false;
    let mut has_yi = false;

    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let ch = bytes[i];
        if ch <= 0x7F {
            if ch.is_ascii_digit() || ch == b'.' {
                filtered.push(ch as char);
            }
            i += 1;
            continue;
        }

        // 检查 UTF-8 多字节字符
        if bytes.len() - i >= 3 {
            // "万" = E4 B8 87
            if bytes[i] == 0xE4 && bytes[i + 1] == 0xB8 && bytes[i + 2] == 0x87 {
                has_wan = true;
                i += 3;
                continue;
            }
            // "亿" = E4 BA BF
            if bytes[i] == 0xE4 && bytes[i + 1] == 0xBA && bytes[i + 2] == 0xBF {
                has_yi = true;
                i += 3;
                continue;
            }
        }

        // 跳过其他 UTF-8 字符
        let advance = if (ch & 0xE0) == 0xC0 {
            2
        } else if (ch & 0xF0) == 0xE0 {
            3
        } else if (ch & 0xF8) == 0xF0 {
            4
        } else {
            1
        };
        i += advance;
    }

    if filtered.is_empty() {
        return None;
    }

    let base: f64 = filtered.parse().ok()?;
    let mut scale = 1.0;
    if has_yi {
        scale *= 100_000_000.0;
    }
    if has_wan {
        scale *= 10_000.0;
    }

    let result = base * scale;
    if result < 0.0 {
        return None;
    }

    Some(result.round() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_online_count() {
        assert_eq!(format_online_count(500), "500");
        assert_eq!(format_online_count(1500), "1000+");
        assert_eq!(format_online_count(15000), "1.50万+");
        assert_eq!(format_online_count(150000), "15.0万+");
        assert_eq!(format_online_count(1500000), "150万+");
    }

    #[test]
    fn test_parse_online_total_string() {
        assert_eq!(parse_online_total_string("1234"), Some(1234));
        assert_eq!(parse_online_total_string("1.5万"), Some(15000));
        assert_eq!(parse_online_total_string("2亿"), Some(200_000_000));
    }
}
