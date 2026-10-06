mod assets;
mod cmg;
mod config;
mod constants;
mod flow;
mod live;
mod media;
mod playlist;
mod prefix;
mod sdk;
mod sign;
mod ticket;
mod ts_decrypt;
mod ts_remux;

use std::{
    collections::HashMap,
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::Result;
use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Redirect, Response},
    routing::get,
    Json, Router,
};
use clap::Parser;
use serde::Serialize;
use tokio::sync::Mutex;
use tower_http::cors::CorsLayer;
use tracing::{info, warn};

use crate::{
    config::ChannelDirectory,
    constants::{DEFAULT_HOST, DEFAULT_PORT, NOTICE_CACHE_TTL_MS, NOTICE_URL},
    live::LiveClient,
    media::MediaPipeline,
    playlist::build_list_m3u,
};

#[derive(Debug, Parser)]
#[command(name = "iptv-rust", about = "Single-binary IPTV relay")]
struct Args {
    #[arg(long, default_value = DEFAULT_HOST)]
    host: String,
    #[arg(long, default_value_t = DEFAULT_PORT)]
    port: u16,
    #[arg(long, default_value = "/app/channels.yaml")]
    channels: PathBuf,
    #[arg(long, default_value_t = false)]
    verbose: bool,
}

#[derive(Clone)]
struct AppState {
    channels_path: PathBuf,
    live: LiveClient,
    media: MediaPipeline,
    notice_cache: Arc<Mutex<HashMap<String, u128>>>,
    stats: Arc<Mutex<Stats>>,
}

#[derive(Debug, Clone, Default, Serialize)]
struct Stats {
    started_at_ms: u128,
    playlist_requests: u64,
    segment_requests: u64,
    live_info_fetches: u64,
    segment_streamed: u64,
    segment_errors: u64,
}

#[derive(Debug, Serialize)]
struct Health {
    ok: bool,
    mode: &'static str,
    stats: Stats,
    channels: ChannelHealth,
    notice: NoticeHealth,
    api_flow: flow::FlowSnapshot,
    routes: Vec<&'static str>,
}

#[derive(Debug, Serialize)]
struct ChannelHealth {
    path: String,
    count: usize,
}

#[derive(Debug, Serialize)]
struct NoticeHealth {
    url: &'static str,
    ttl_ms: u64,
    cache: HashMap<String, NoticeCacheItem>,
}

#[derive(Debug, Serialize)]
struct NoticeCacheItem {
    expires_at_ms: u128,
    ttl_ms: u128,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let filter = if args.verbose { "info" } else { "warn" };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| filter.into()),
        )
        .init();

    let live = LiveClient::new()?;
    let media = MediaPipeline::new(live.clone())?;
    let state = AppState {
        channels_path: args.channels.clone(),
        live,
        media,
        notice_cache: Arc::new(Mutex::new(HashMap::new())),
        stats: Arc::new(Mutex::new(Stats {
            started_at_ms: now_ms(),
            ..Stats::default()
        })),
    };

    // Fail fast if the packaged YAML is missing or malformed.
    let initial_channels = ChannelDirectory::load(&state.channels_path)?;
    info!(
        path = %initial_channels.path.display(),
        count = initial_channels.channels.len(),
        "loaded channel directory"
    );

    let app = Router::new()
        .route("/health", get(health))
        .route("/channels", get(channels))
        .route("/list.m3u", get(list_m3u))
        .route("/live/{file}", get(live_playlist))
        .route("/segment/{ch}/{file}", get(segment))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let addr: SocketAddr = format!("{}:{}", args.host, args.port).parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(%addr, "iptv-rust listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn health(State(state): State<AppState>) -> Response {
    let directory = match load_channels(&state) {
        Ok(value) => value,
        Err(error) => {
            return json_status(
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({ "ok": false, "error": error.to_string() }),
            );
        }
    };
    let now = now_ms();
    let notice_cache = state.notice_cache.lock().await;
    let notice = notice_cache
        .iter()
        .map(|(ch, expires_at)| {
            (
                ch.clone(),
                NoticeCacheItem {
                    expires_at_ms: *expires_at,
                    ttl_ms: expires_at.saturating_sub(now),
                },
            )
        })
        .collect();
    let api_flow = state.live.flow.snapshot().await;
    let mut stats = state.stats.lock().await.clone();
    stats.live_info_fetches = api_flow.stats.completed;
    Json(Health {
        ok: true,
        mode: "rust-staged-mpegts",
        stats,
        channels: ChannelHealth {
            path: directory.path.display().to_string(),
            count: directory.channels.len(),
        },
        notice: NoticeHealth {
            url: NOTICE_URL,
            ttl_ms: NOTICE_CACHE_TTL_MS,
            cache: notice,
        },
        api_flow,
        routes: vec![
            "/list.m3u",
            "/live/<ch>.m3u8",
            "/segment/<ch>/<id>.ts",
            "/channels",
            "/health",
        ],
    })
    .into_response()
}

async fn channels(State(state): State<AppState>) -> Response {
    match load_channels(&state) {
        Ok(directory) => Json(serde_json::json!({
            "ok": true,
            "path": directory.path,
            "count": directory.channels.len(),
            "channels": directory.channels,
        }))
        .into_response(),
        Err(error) => json_status(
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({ "ok": false, "error": error.to_string() }),
        ),
    }
}

async fn list_m3u(State(state): State<AppState>, headers: HeaderMap, uri: Uri) -> Response {
    match load_channels(&state) {
        Ok(directory) => text_response(
            StatusCode::OK,
            "application/vnd.apple.mpegurl; charset=utf-8",
            build_list_m3u(&headers, &uri, &directory.channels),
        ),
        Err(error) => json_status(
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({ "ok": false, "error": error.to_string() }),
        ),
    }
}

async fn live_playlist(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Path(file): Path<String>,
) -> Response {
    let Some(ch) = strip_suffix(&file, ".m3u8") else {
        return json_status(
            StatusCode::NOT_FOUND,
            serde_json::json!({ "ok": false, "error": "not found" }),
        );
    };
    {
        let mut stats = state.stats.lock().await;
        stats.playlist_requests += 1;
    }
    let directory = match load_channels(&state) {
        Ok(value) => value,
        Err(error) => return temporary_notice(&state, ch, error).await,
    };
    let Some(channel) = directory.resolve_ch(&ch) else {
        return Redirect::temporary(NOTICE_URL).into_response();
    };
    if notice_cached(&state, &channel.ch).await {
        return Redirect::temporary(NOTICE_URL).into_response();
    }
    match state
        .media
        .local_ts_playlist(&channel, &headers, &uri)
        .await
    {
        Ok(text) => text_response(
            StatusCode::OK,
            "application/vnd.apple.mpegurl; charset=utf-8",
            text,
        ),
        Err(error) => temporary_notice(&state, &channel.ch, error).await,
    }
}

async fn segment(
    State(state): State<AppState>,
    Path((ch, file)): Path<(String, String)>,
) -> Response {
    let Some(id) = strip_suffix(&file, ".ts") else {
        return json_status(
            StatusCode::NOT_FOUND,
            serde_json::json!({ "ok": false, "error": "not found" }),
        );
    };
    {
        let mut stats = state.stats.lock().await;
        stats.segment_requests += 1;
    }
    let directory = match load_channels(&state) {
        Ok(value) => value,
        Err(error) => {
            return json_status(
                StatusCode::BAD_GATEWAY,
                serde_json::json!({ "ok": false, "error": error.to_string() }),
            )
        }
    };
    let Some(channel) = directory.resolve_ch(&ch) else {
        return Redirect::temporary(NOTICE_URL).into_response();
    };
    match state.media.segment(&channel, id).await {
        Ok(body) => {
            let mut response = body.into_response();
            response
                .headers_mut()
                .insert(header::CONTENT_TYPE, "video/mp2t".parse().unwrap());
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                "public, max-age=300".parse().unwrap(),
            );
            {
                let mut stats = state.stats.lock().await;
                stats.segment_streamed += 1;
            }
            response
        }
        Err(error) => {
            let mut stats = state.stats.lock().await;
            stats.segment_errors += 1;
            drop(stats);
            json_status(
                StatusCode::BAD_GATEWAY,
                serde_json::json!({ "ok": false, "error": error.to_string() }),
            )
        }
    }
}

fn strip_suffix<'a>(value: &'a str, suffix: &str) -> Option<&'a str> {
    value.strip_suffix(suffix).filter(|value| !value.is_empty())
}

fn load_channels(state: &AppState) -> Result<ChannelDirectory> {
    ChannelDirectory::load(&state.channels_path)
}

async fn notice_cached(state: &AppState, ch: &str) -> bool {
    let now = now_ms();
    let mut cache = state.notice_cache.lock().await;
    let key = ch.to_ascii_lowercase();
    if let Some(expires_at) = cache.get(&key).copied() {
        if expires_at > now {
            return true;
        }
        cache.remove(&key);
    }
    false
}

async fn temporary_notice(state: &AppState, ch: &str, error: anyhow::Error) -> Response {
    warn!(channel = %ch, error = %error, "temporary notice fallback");
    let mut cache = state.notice_cache.lock().await;
    cache.insert(
        ch.to_ascii_lowercase(),
        now_ms() + NOTICE_CACHE_TTL_MS as u128,
    );
    Redirect::temporary(NOTICE_URL).into_response()
}

fn text_response(status: StatusCode, content_type: &'static str, text: String) -> Response {
    let mut response = (status, text).into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, content_type.parse().unwrap());
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
}

fn json_status(status: StatusCode, value: serde_json::Value) -> Response {
    (status, Json(value)).into_response()
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis()
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        let mut signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        signal.recv().await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
