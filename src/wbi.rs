//! WBI 签名（按 bilibili-API-collect docs/misc/sign/wbi.md）

use reqwest::Client;
use serde::Deserialize;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::warn;

const MIXIN_KEY_ENC_TAB: [usize; 64] = [
    46, 47, 18, 2, 53, 8, 23, 32, 15, 50, 10, 31, 58, 3, 45, 35, 27, 43, 5, 49, 33, 9, 42, 19, 29,
    28, 14, 39, 12, 38, 41, 13, 37, 48, 7, 16, 24, 55, 40, 61, 26, 17, 0, 1, 60, 51, 30, 4, 22, 25,
    54, 21, 56, 59, 6, 63, 57, 62, 11, 36, 20, 34, 44, 52,
];

#[derive(Deserialize)]
struct WbiImg {
    img_url: String,
    sub_url: String,
}

#[derive(Deserialize)]
struct NavData {
    wbi_img: WbiImg,
}

#[derive(Deserialize)]
struct NavResponse {
    data: NavData,
}

/// 对 imgKey + subKey 按映射表打乱，取前 32 位得到 mixin_key
fn get_mixin_key(orig: &[u8]) -> String {
    MIXIN_KEY_ENC_TAB
        .iter()
        .take(32)
        .map(|&i| orig[i] as char)
        .collect()
}

/// 百分号编码（大写 hex，空格为 %20；过滤 !'()*）
fn get_url_encoded(s: &str) -> String {
    s.chars()
        .filter_map(|c| {
            if c.is_ascii_alphanumeric() || "-_.~".contains(c) {
                Some(c.to_string())
            } else if "!'()*".contains(c) {
                None
            } else {
                let mut buf = [0u8; 4];
                let encoded = c
                    .encode_utf8(&mut buf)
                    .bytes()
                    .fold(String::new(), |acc, b| acc + &format!("%{b:02X}"));
                Some(encoded)
            }
        })
        .collect()
}

fn take_filename(url: &str) -> Option<String> {
    url.rsplit_once('/')
        .and_then(|(_, s)| s.rsplit_once('.'))
        .map(|(s, _)| s.to_string())
}

fn encode_wbi_with_timestamp(
    mut params: Vec<(&str, String)>,
    (img_key, sub_key): (String, String),
    timestamp: u64,
) -> String {
    let mixin_key = get_mixin_key((img_key + &sub_key).as_bytes());
    params.push(("wts", timestamp.to_string()));
    params.sort_by(|a, b| a.0.cmp(b.0));

    let query = params
        .iter()
        .map(|(k, v)| format!("{}={}", get_url_encoded(k), get_url_encoded(v)))
        .collect::<Vec<_>>()
        .join("&");

    let web_sign = format!("{:x}", md5::compute(query.clone() + &mixin_key));
    format!("{query}&w_rid={web_sign}")
}

/// 为请求参数附加 WBI 签名，返回完整 query string
pub fn encode_wbi(params: Vec<(&str, String)>, keys: (String, String)) -> String {
    let cur_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|t| t.as_secs())
        .unwrap_or(0);
    encode_wbi_with_timestamp(params, keys, cur_time)
}

/// 从 nav 接口获取 img_key / sub_key（未登录 code=-101 也可拿到）
pub async fn get_wbi_keys(client: &Client, cookie: &str) -> Option<(String, String)> {
    let response = client
        .get("https://api.bilibili.com/x/web-interface/nav")
        .header("Referer", "https://www.bilibili.com/")
        .header("Cookie", cookie)
        .send()
        .await
        .ok()?;

    if !response.status().is_success() {
        warn!("[bili] nav api returned status {}", response.status());
        return None;
    }

    let nav: NavResponse = match response.json().await {
        Ok(v) => v,
        Err(e) => {
            warn!("[bili] nav api json parse failed: {}", e);
            return None;
        }
    };

    let img_key = take_filename(&nav.data.wbi_img.img_url)?;
    let sub_key = take_filename(&nav.data.wbi_img.sub_url)?;
    Some((img_key, sub_key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_filename() {
        assert_eq!(
            take_filename("https://i0.hdslb.com/bfs/wbi/7cd084941338484aae1ad9425b84077c.png"),
            Some("7cd084941338484aae1ad9425b84077c".to_string())
        );
    }

    #[test]
    fn test_get_mixin_key() {
        let concat_key =
            "7cd084941338484aae1ad9425b84077c".to_string() + "4932caff0ff746eab6f01bf08b70ac45";
        assert_eq!(
            get_mixin_key(concat_key.as_bytes()),
            "ea1db124af3c7062474693fa704f4ff8"
        );
    }

    #[test]
    fn test_encode_wbi() {
        let params = vec![
            ("foo", String::from("114")),
            ("bar", String::from("514")),
            ("zab", String::from("1919810")),
        ];
        assert_eq!(
            encode_wbi_with_timestamp(
                params,
                (
                    "7cd084941338484aae1ad9425b84077c".to_string(),
                    "4932caff0ff746eab6f01bf08b70ac45".to_string()
                ),
                1702204169
            ),
            "bar=514&foo=114&wts=1702204169&zab=1919810&w_rid=8f6f2b5b3d485fe1886cec6a0be8c5d4"
        );
    }
}
