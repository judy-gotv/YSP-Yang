use anyhow::{Context, Result};
use base64::Engine;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::{
    config::Channel,
    constants::{
        ACTIVE_URL, API_FLOW_JITTER_MS, API_FLOW_MIN_INTERVAL_MS, API_FLOW_QUEUE_TIMEOUT_MS,
        API_FLOW_RETRIES, API_FLOW_RETRY_DELAY_MS, APP_VER, AUTH_SECRET, CHANNEL, DEFAULT_DEFN,
        DEFAULT_STREAM, LIVE_SECRET, M3U8_REFRESH_AFTER_MS, M3U8_STALE_GRACE_MS, M3U8_TTL_MS,
        PLATFORM, PLAYER_API, USER_AGENT, YSPAPPID,
    },
    flow::{ApiFlowLimiter, FlowOptions},
    sdk::{
        build_input, build_request_id, canonical_body_md5, fetch_openapi_token, sign_with_token,
        SdkState,
    },
    sign::{build_ckey, generate_guid, md5_js_default_sorted_with_secret, random_string},
    ticket::build_ticket,
};

#[derive(Clone)]
pub struct LiveClient {
    http: Client,
    pub flow: ApiFlowLimiter,
    stream: String,
    defn: String,
    cache: Arc<Mutex<SourceCacheState>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceCacheEntry {
    pub ch: String,
    pub cnlid: String,
    pub livepid: String,
    pub cache_key: String,
    pub guid: String,
    pub url: String,
    pub fetched_at_ms: u128,
    pub refresh_after_ms: u128,
    pub expires_at_ms: u128,
    pub stale_until_ms: u128,
}

#[derive(Default)]
struct SourceCacheState {
    entries: HashMap<String, SourceCacheEntry>,
    refresh_inflight: HashSet<String>,
}

#[derive(Debug)]
enum CacheLookup {
    Hit {
        entry: SourceCacheEntry,
        refresh: bool,
    },
    Miss,
}

impl LiveClient {
    pub fn new() -> Result<Self> {
        let http = Client::builder()
            .user_agent(crate::constants::USER_AGENT)
            .timeout(Duration::from_secs(20))
            .build()?;
        Ok(Self {
            http,
            flow: ApiFlowLimiter::new(FlowOptions {
                concurrency: crate::constants::API_FLOW_CONCURRENCY,
                min_interval: Duration::from_millis(API_FLOW_MIN_INTERVAL_MS),
                jitter: Duration::from_millis(API_FLOW_JITTER_MS),
                queue_timeout: Duration::from_millis(API_FLOW_QUEUE_TIMEOUT_MS),
                retries: API_FLOW_RETRIES,
                retry_delay: Duration::from_millis(API_FLOW_RETRY_DELAY_MS),
            }),
            stream: DEFAULT_STREAM.to_string(),
            defn: DEFAULT_DEFN.to_string(),
            cache: Arc::new(Mutex::new(SourceCacheState::default())),
        })
    }

    pub async fn fetch_source(&self, channel: Channel) -> Result<SourceCacheEntry> {
        let cache_key = channel.cache_key();
        if let Some(cached) = self.cached_source_or_refresh(&channel, &cache_key).await {
            return Ok(cached);
        }
        let value = self.refresh_source(channel).await?;
        Ok(value)
    }

    pub async fn invalidate_source(&self, cache_key: &str) {
        let mut cache = self.cache.lock().await;
        if let Some(entry) = cache.entries.get_mut(cache_key) {
            let now = now_epoch_ms();
            entry.refresh_after_ms = now;
            entry.expires_at_ms = now;
        }
    }

    pub async fn refresh_source_background(&self, channel: Channel) {
        self.spawn_refresh_if_needed(channel).await;
    }

    pub async fn refresh_source_now(&self, channel: Channel) -> Result<SourceCacheEntry> {
        self.refresh_source(channel).await
    }

    async fn cached_source_or_refresh(
        &self,
        channel: &Channel,
        cache_key: &str,
    ) -> Option<SourceCacheEntry> {
        let now = now_epoch_ms();
        let lookup = {
            let mut cache = self.cache.lock().await;
            lookup_cached_source(&mut cache, cache_key, now)
        };
        match lookup {
            CacheLookup::Hit { entry, refresh } => {
                if refresh {
                    self.spawn_refresh_if_needed(channel.clone()).await;
                }
                Some(entry)
            }
            CacheLookup::Miss => None,
        }
    }

    async fn spawn_refresh_if_needed(&self, channel: Channel) {
        let cache_key = channel.cache_key();
        {
            let mut cache = self.cache.lock().await;
            if !cache.refresh_inflight.insert(cache_key.clone()) {
                return;
            }
        }
        let this = self.clone();
        tokio::spawn(async move {
            match this.refresh_source(channel).await {
                Ok(entry) => {
                    debug!(channel = %entry.ch, cache_key = %entry.cache_key, "refreshed live source cache");
                }
                Err(error) => {
                    warn!(cache_key = %cache_key, error = %error_chain(&error), "background live source refresh failed");
                    this.cache.lock().await.refresh_inflight.remove(&cache_key);
                }
            }
        });
    }

    async fn refresh_source(&self, channel: Channel) -> Result<SourceCacheEntry> {
        let cache_key = channel.cache_key();
        let this = self.clone();
        let result = self
            .flow
            .run(async move {
                let mut last_error = None;
                for attempt in 0..=API_FLOW_RETRIES {
                    match this.fetch_source_once(&channel).await {
                        Ok(value) => return Ok(value),
                        Err(error) => {
                            let retryable = is_retryable_error(&error);
                            last_error = Some(error);
                            if attempt >= API_FLOW_RETRIES || !retryable {
                                break;
                            }
                            let delay = Duration::from_millis(
                                API_FLOW_RETRY_DELAY_MS * (attempt as u64 + 1),
                            );
                            tokio::time::sleep(delay).await;
                        }
                    }
                }
                Err(last_error.unwrap_or_else(|| anyhow::anyhow!("get_live_info failed")))
            })
            .await;
        let mut cache = self.cache.lock().await;
        cache.refresh_inflight.remove(&cache_key);
        let value = result?;
        cache.entries.insert(cache_key, value.clone());
        Ok(value)
    }

    async fn fetch_source_once(&self, channel: &Channel) -> Result<SourceCacheEntry> {
        let guid = generate_guid();
        let request_ts = chrono_like_unix_seconds();
        let ckey = build_ckey(&channel.cnlid, request_ts, &guid)?;
        let auth = self.auth_request(&channel.livepid, &guid).await?;
        let sign_body = self.build_live_sign_body(channel, &guid, &ckey.ckey);
        let body_md5 = canonical_body_md5(
            sign_body
                .iter()
                .map(|(key, value)| (key.clone(), json_scalar_to_string(value))),
        );
        let request_id = build_request_id();
        let sdk_input = build_input(&body_md5, &guid, 1, &request_id);
        let sdk_token = fetch_openapi_token(&self.http, &guid).await?;
        let mut sdk_state = SdkState::new(&guid, sdk_token.token.clone(), sdk_input);
        sdk_state.ts = sdk_token.ts.clone();
        let sdk_headers = sign_with_token(sdk_state, 1, request_id)?;
        let ticket = build_ticket(&channel.livepid, &auth.ts, &channel.cnlid, &guid)?;

        let mut body = sign_body;
        body.insert(
            "rand_str".to_string(),
            serde_json::Value::String(random_string(10)),
        );
        let signature = md5_js_default_sorted_with_secret(
            body.iter()
                .map(|(key, value)| (key.clone(), json_scalar_to_string(value))),
            LIVE_SECRET,
        );
        body.insert(
            "signature".to_string(),
            serde_json::Value::String(signature),
        );

        let url = format!("{PLAYER_API}v1/player/get_live_info");
        let response = self
            .http
            .post(url)
            .headers(base_headers(&guid)?)
            .header("yspappid", YSPAPPID)
            .header("content-type", "application/json;charset=UTF-8")
            .header("yspsdkinput", sdk_headers.yspsdkinput)
            .header("yspsdksign", sdk_headers.yspsdksign)
            .header("seqId", sdk_headers.seq_id.to_string())
            .header("request-id", sdk_headers.request_id)
            .header("yspPlayerToken", auth.token)
            .header("yspticket", ticket)
            .json(&body)
            .send()
            .await?;
        let status = response.status();
        let headers = format!("{:?}", response.headers());
        let text = response.text().await?;
        if !status.is_success() {
            anyhow::bail!(
                "get_live_info failed status={} headers={} body={} body_hex={}",
                status.as_u16(),
                headers,
                text.chars().take(1000).collect::<String>(),
                hex_head(text.as_bytes())
            );
        }
        let parsed: LiveInfoResponse = serde_json::from_str(&text).with_context(|| {
            format!(
                "parse get_live_info response status={} headers={} body={} body_hex={}",
                status.as_u16(),
                headers,
                text.chars().take(1000).collect::<String>(),
                hex_head(text.as_bytes())
            )
        })?;
        let data = parsed.data.unwrap_or_default();
        if parsed.code != 0 || data.iretcode != 0 || data.playurl.is_empty() {
            anyhow::bail!(
                "get_live_info failed code={} iretcode={}: {}",
                parsed.code,
                data.iretcode,
                text.chars().take(1000).collect::<String>()
            );
        }

        let now = now_epoch_ms();
        Ok(SourceCacheEntry {
            ch: channel.ch.clone(),
            cnlid: channel.cnlid.clone(),
            livepid: channel.livepid.clone(),
            cache_key: channel.cache_key(),
            guid,
            url: build_playback_url(&data),
            fetched_at_ms: now,
            refresh_after_ms: now + M3U8_REFRESH_AFTER_MS as u128,
            expires_at_ms: now + M3U8_TTL_MS as u128,
            stale_until_ms: now + M3U8_TTL_MS as u128 + M3U8_STALE_GRACE_MS as u128,
        })
    }

    async fn auth_request(&self, pid: &str, guid: &str) -> Result<AuthData> {
        let mut body = HashMap::from([
            ("pid".to_string(), pid.to_string()),
            ("guid".to_string(), guid.to_string()),
            ("appid".to_string(), "ysp_pc".to_string()),
            ("rand_str".to_string(), random_string(10)),
        ]);
        let signature = md5_js_default_sorted_with_secret(
            body.iter().map(|(key, value)| (key.clone(), value.clone())),
            AUTH_SECRET,
        );
        body.insert("signature".to_string(), signature);
        let url = format!("{PLAYER_API}v1/player/auth");
        let response = self
            .http
            .post(url)
            .headers(base_headers(guid)?)
            .header("yspappid", YSPAPPID)
            .header(
                "content-type",
                "application/x-www-form-urlencoded;charset=UTF-8",
            )
            .form(&body)
            .send()
            .await?;
        let status = response.status();
        let text = response.text().await?;
        let parsed: AuthResponse = serde_json::from_str(&text).with_context(|| {
            format!(
                "parse auth response: {}",
                text.chars().take(500).collect::<String>()
            )
        })?;
        let data = parsed.data.unwrap_or_default();
        if !status.is_success() || parsed.code != 0 || data.token.is_empty() {
            anyhow::bail!(
                "auth failed status={} code={}: {}",
                status.as_u16(),
                parsed.code,
                text.chars().take(500).collect::<String>()
            );
        }
        Ok(data)
    }

    fn build_live_sign_body(
        &self,
        channel: &Channel,
        guid: &str,
        ckey: &str,
    ) -> serde_json::Map<String, serde_json::Value> {
        let body = serde_json::json!({
            "cnlid": channel.cnlid,
            "livepid": channel.livepid,
            "stream": self.stream,
            "guid": guid,
            "cKey": ckey,
            "adjust": 1,
            "sphttps": "1",
            "platform": PLATFORM,
            "cmd": "2",
            "encryptVer": "8.1",
            "dtype": "1",
            "devid": "devid",
            "otype": "ojson",
            "appVer": APP_VER,
            "app_version": APP_VER,
            "channel": CHANNEL,
            "defn": self.defn,
        });
        body.as_object().expect("json object").clone()
    }
}

#[derive(Debug, Deserialize)]
struct AuthResponse {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    data: Option<AuthData>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct AuthData {
    #[serde(default)]
    token: String,
    #[serde(default)]
    ts: String,
}

#[derive(Debug, Deserialize)]
struct LiveInfoResponse {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    data: Option<LiveInfoData>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct LiveInfoData {
    #[serde(default)]
    iretcode: i64,
    #[serde(default)]
    playurl: String,
    #[serde(default)]
    extended_param: String,
    #[serde(default)]
    chanll: Option<serde_json::Value>,
}

fn base_headers(guid: &str) -> Result<reqwest::header::HeaderMap> {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("accept", "application/json, text/plain, */*".parse()?);
    headers.insert("origin", ACTIVE_URL.parse()?);
    headers.insert("referer", format!("{ACTIVE_URL}/").parse()?);
    headers.insert("user-agent", USER_AGENT.parse()?);
    headers.insert("cookie", build_cookie(guid).parse()?);
    Ok(headers)
}

fn build_cookie(guid: &str) -> String {
    [
        format!("guid={guid}"),
        "versionName=99.99.99".to_string(),
        "versionCode=999999".to_string(),
        "vplatform=109".to_string(),
        "platformVersion=Chrome".to_string(),
        "deviceModel=148".to_string(),
        "newLogin=1".to_string(),
        "pc_version=1.1.16".to_string(),
    ]
    .join("; ")
}

fn build_playback_url(data: &LiveInfoData) -> String {
    let revoi = decode_revoi(data.chanll.as_ref());
    let mut url = format!("{}&revoi={}", data.playurl, revoi);
    if !data.extended_param.is_empty() {
        url.push_str(&data.extended_param);
    }
    url
}

fn decode_revoi(chanll: Option<&serde_json::Value>) -> String {
    let Some(chanll) = chanll else {
        return String::new();
    };
    let code = if let Some(code) = chanll.get("code").and_then(|value| value.as_str()) {
        code.to_string()
    } else if let Some(text) = chanll.as_str() {
        serde_json::from_str::<serde_json::Value>(text)
            .ok()
            .and_then(|value| {
                value
                    .get("code")
                    .and_then(|code| code.as_str())
                    .map(ToOwned::to_owned)
            })
            .unwrap_or_default()
    } else {
        String::new()
    };
    if code.is_empty() {
        return String::new();
    }
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(code) else {
        return String::new();
    };
    let decoded = String::from_utf8_lossy(&bytes);
    let trimmed = decoded.trim();
    if let Ok(value) = serde_json::from_str::<String>(trimmed) {
        return value;
    }
    String::new()
}

fn lookup_cached_source(cache: &mut SourceCacheState, cache_key: &str, now: u128) -> CacheLookup {
    let Some(entry) = cache.entries.get(cache_key).cloned() else {
        return CacheLookup::Miss;
    };
    if entry.expires_at_ms > now {
        return CacheLookup::Hit {
            refresh: entry.refresh_after_ms <= now,
            entry,
        };
    }
    if entry.stale_until_ms > now {
        return CacheLookup::Hit {
            entry,
            refresh: true,
        };
    }
    cache.entries.remove(cache_key);
    CacheLookup::Miss
}

fn json_scalar_to_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(value) => value.clone(),
        serde_json::Value::Number(value) => value.to_string(),
        serde_json::Value::Bool(value) => value.to_string(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn chrono_like_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn now_epoch_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn hex_head(bytes: &[u8]) -> String {
    hex::encode(&bytes[..bytes.len().min(64)])
}

fn is_retryable_error(error: &anyhow::Error) -> bool {
    let message = error.to_string();
    [
        "408",
        "425",
        "429",
        "500",
        "502",
        "503",
        "504",
        "timeout",
        "timed out",
        "rate",
        "limit",
    ]
    .iter()
    .any(|needle| message.to_ascii_lowercase().contains(needle))
}

fn error_chain(error: &anyhow::Error) -> String {
    error
        .chain()
        .map(|cause| cause.to_string())
        .collect::<Vec<_>>()
        .join("; caused by: ")
}

#[allow(dead_code)]
fn now_ms(start: Instant) -> u128 {
    start.elapsed().as_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entry(now: u128) -> SourceCacheEntry {
        SourceCacheEntry {
            ch: "cctv1".to_string(),
            cnlid: "2024078201".to_string(),
            livepid: "600001859".to_string(),
            cache_key: "2024078201:600001859".to_string(),
            guid: "guid".to_string(),
            url: "https://example.test/live.m3u8".to_string(),
            fetched_at_ms: now,
            refresh_after_ms: now + 10,
            expires_at_ms: now + 20,
            stale_until_ms: now + 40,
        }
    }

    #[test]
    fn stale_source_is_returned_while_refresh_is_needed() {
        let mut cache = SourceCacheState::default();
        let entry = sample_entry(100);
        cache.entries.insert(entry.cache_key.clone(), entry.clone());

        match lookup_cached_source(&mut cache, &entry.cache_key, 125) {
            CacheLookup::Hit {
                entry: hit,
                refresh,
            } => {
                assert!(refresh);
                assert_eq!(hit.url, entry.url);
            }
            CacheLookup::Miss => panic!("stale source should still be usable"),
        }
    }

    #[test]
    fn source_is_removed_after_stale_grace() {
        let mut cache = SourceCacheState::default();
        let entry = sample_entry(100);
        cache.entries.insert(entry.cache_key.clone(), entry.clone());

        assert!(matches!(
            lookup_cached_source(&mut cache, &entry.cache_key, 145),
            CacheLookup::Miss
        ));
        assert!(!cache.entries.contains_key(&entry.cache_key));
    }
}
