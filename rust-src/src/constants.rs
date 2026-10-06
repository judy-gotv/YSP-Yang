pub const PLAYER_API: &str = "https://player-api.yangshipin.cn/";
pub const PAGE_URL: &str = "https://www.yangshipin.cn/tv/home?pid=600099502";
pub const ACTIVE_URL: &str = "https://www.yangshipin.cn";
pub const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/148.0.0.0 Safari/537.36";

pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 8787;
pub const DEFAULT_STREAM: &str = "2";
pub const DEFAULT_DEFN: &str = "fhd";

pub const YSPAPPID: &str = "519748109";
pub const PLATFORM: &str = "5910204";
pub const APP_VER: &str = "V1.0.0";
pub const CHANNEL: &str = "ysp_tx";
pub const AUTH_SECRET: &str = "n@7QKk%YeSjfw%22";
pub const LIVE_SECRET: &str = "0f$IVHi9Qno?G";
pub const CKEY_AES_KEY_HEX: &str = "48e5918a74ae21c972b90cce8af6c8be";
pub const CKEY_AES_IV_HEX: &str = "9a7e7d23610266b1d9fbf98581384d92";

pub const NOTICE_URL: &str = "https://cdn.jsdelivr.net/gh/jkwu5472/first/media.m3u8";
pub const NOTICE_LOGO_URL: &str = "https://cdn.jsdelivr.net/gh/jkwu5472/first/notice.jpg";
pub const EPG_URL: &str = "https://epg.zsdc.eu.org/t.xml";

pub const M3U8_REFRESH_AFTER_MS: u64 = 60_000;
pub const M3U8_TTL_MS: u64 = 180_000;
pub const M3U8_STALE_GRACE_MS: u64 = 300_000;
pub const NOTICE_CACHE_TTL_MS: u64 = 60_000;
pub const MEDIA_PLAYLIST_SNAPSHOT_TTL_MS: u64 = 4_000;
pub const MEDIA_PLAYLIST_WINDOW_SEGMENTS: usize = 12;
pub const MEDIA_HISTORY_MAX_SEGMENTS: usize = 36;
pub const MEDIA_LIVE_EDGE_HOLDBACK_SEGMENTS: usize = 1;

pub const API_FLOW_CONCURRENCY: usize = 1;
pub const API_FLOW_MIN_INTERVAL_MS: u64 = 900;
pub const API_FLOW_JITTER_MS: u64 = 300;
pub const API_FLOW_QUEUE_TIMEOUT_MS: u64 = 600_000;
pub const API_FLOW_RETRIES: usize = 2;
pub const API_FLOW_RETRY_DELAY_MS: u64 = 2_500;
