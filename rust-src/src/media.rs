use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Context, Result};
use axum::{
    body::Body,
    http::{HeaderMap, Uri},
};
use bytes::Bytes;
use reqwest::Client;
use serde::Serialize;
use sha1::{Digest, Sha1};
use tokio::sync::Mutex;
use tracing::{info, warn};
use url::Url;

use crate::{
    cmg::CmgRuntime,
    config::Channel,
    constants::{
        ACTIVE_URL, MEDIA_HISTORY_MAX_SEGMENTS, MEDIA_LIVE_EDGE_HOLDBACK_SEGMENTS,
        MEDIA_PLAYLIST_WINDOW_SEGMENTS, USER_AGENT,
    },
    live::LiveClient,
    prefix::{abs_url, append_recursive_prefix},
    ts_remux::{decrypt_and_remux_ts, CmgVideoState, RemuxStats, TsMuxState},
};

#[derive(Clone)]
pub struct MediaPipeline {
    live: LiveClient,
    http: Client,
    state: Arc<Mutex<MediaState>>,
}

#[derive(Default)]
struct MediaState {
    segments: HashMap<String, SegmentRef>,
    history: HashMap<String, Vec<SegmentRef>>,
    runtimes: HashMap<String, Arc<Mutex<ChannelRuntime>>>,
}

#[derive(Debug, Clone)]
struct SegmentRef {
    id: String,
    ch: String,
    livepid: String,
    url: String,
    duration: f64,
    sequence: i64,
}

struct ChannelRuntime {
    media_tag_id: String,
    page_url: String,
    cmg: CmgRuntime,
    video_state: CmgVideoState,
    mux_state: TsMuxState,
    processed: HashMap<i64, ProcessedSegment>,
    last_processed_sequence: Option<i64>,
    reset_count: u64,
}

#[derive(Clone)]
struct ProcessedSegment {
    bytes: Bytes,
    stats: RemuxStats,
}

#[derive(Serialize)]
struct SegmentDumpMeta<'a> {
    ch: &'a str,
    livepid: &'a str,
    sequence: i64,
    id: &'a str,
    url: &'a str,
    media_tag_id: &'a str,
    vmp_tag: &'a str,
    reset_count: u64,
    input_bytes: usize,
    output_bytes: usize,
    stats: RemuxStats,
}

#[derive(Debug)]
struct MediaPlaylist {
    url: String,
    media_sequence: i64,
    segments: Vec<ParsedSegment>,
}

#[derive(Debug, Clone)]
struct ParsedSegment {
    url: String,
    duration: f64,
    sequence: i64,
}

#[derive(Debug)]
struct ParsedM3u8 {
    media_sequence: i64,
    segments: Vec<ParsedSegment>,
    playlists: Vec<String>,
}

impl MediaPipeline {
    pub fn new(live: LiveClient) -> Result<Self> {
        let http = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(std::time::Duration::from_secs(20))
            .build()?;
        Ok(Self {
            live,
            http,
            state: Arc::new(Mutex::new(MediaState::default())),
        })
    }

    pub async fn local_ts_playlist(
        &self,
        channel: &Channel,
        headers: &HeaderMap,
        uri: &Uri,
    ) -> Result<String> {
        let source = self.live.fetch_source(channel.clone()).await?;
        let history = match self.fetch_media_playlist(&source.url).await {
            Ok(playlist) => self.update_history(channel, &playlist).await,
            Err(first_error) => {
                self.live.invalidate_source(&source.cache_key).await;
                let cached_history = self.cached_history(&channel.livepid).await;
                if !cached_history.is_empty() {
                    self.live.refresh_source_background(channel.clone()).await;
                    warn!(
                        ch = %channel.ch,
                        livepid = %channel.livepid,
                        error = %error_chain(&first_error),
                        "serving cached media history while live source refreshes"
                    );
                    cached_history
                } else {
                    let refreshed = self.live.refresh_source_now(channel.clone()).await?;
                    let playlist = self
                        .fetch_media_playlist(&refreshed.url)
                        .await
                        .with_context(|| {
                            format!(
                                "refresh playback source after upstream m3u8 error: {}",
                                error_chain(&first_error)
                            )
                        })?;
                    self.update_history(channel, &playlist).await
                }
            }
        };
        let live_segments = playable_segment_window(&history);
        if live_segments.is_empty() {
            return Err(anyhow!("no playable upstream TS segments available"));
        }
        let target_duration = live_segments
            .iter()
            .map(|segment| segment.duration.max(1.0).ceil() as u64)
            .max()
            .unwrap_or(5)
            .max(1);
        let mut lines = vec![
            "#EXTM3U".to_string(),
            "#EXT-X-VERSION:3".to_string(),
            "#EXT-X-ALLOW-CACHE:NO".to_string(),
            "#EXT-X-INDEPENDENT-SEGMENTS".to_string(),
            format!("#EXT-X-TARGETDURATION:{target_duration}"),
            format!("#EXT-X-MEDIA-SEQUENCE:{}", live_segments[0].sequence),
        ];
        {
            let mut state = self.state.lock().await;
            for segment in &live_segments {
                state.segments.insert(segment.id.clone(), segment.clone());
                lines.push(format!("#EXTINF:{:.3},", segment.duration));
                let url = abs_url(
                    headers,
                    uri,
                    &format!("/segment/{}/{}.ts", channel.ch, segment.id),
                );
                lines.push(append_recursive_prefix(uri, &url));
            }
            trim_map(&mut state.segments, 512);
        }
        Ok(format!("{}\n", lines.join("\n")))
    }

    pub async fn segment(&self, channel: &Channel, id: &str) -> Result<Body> {
        let segment = {
            let state = self.state.lock().await;
            state.segments.get(id).cloned()
        }
        .ok_or_else(|| anyhow!("unknown segment; refresh playlist first"))?;
        if segment.ch != channel.ch || segment.livepid != channel.livepid {
            return Err(anyhow!("segment does not belong to channel"));
        }
        let runtime = self.runtime_for_channel(&segment.livepid).await?;
        let bytes = self.process_segment(runtime, segment).await?;
        Ok(Body::from(bytes))
    }

    async fn runtime_for_channel(&self, livepid: &str) -> Result<Arc<Mutex<ChannelRuntime>>> {
        if let Some(existing) = self.state.lock().await.runtimes.get(livepid).cloned() {
            return Ok(existing);
        }
        let media_tag_id = new_media_tag_id();
        let page_url = format!("{ACTIVE_URL}/tv/home?pid={livepid}");
        let mut cmg = CmgRuntime::load_for_page(&page_url)
            .with_context(|| format!("load CMG runtime for channel {livepid}"))?;
        cmg.prime(&media_tag_id)
            .with_context(|| format!("prime CMG runtime for channel {livepid}"))?;
        let runtime = Arc::new(Mutex::new(ChannelRuntime {
            media_tag_id,
            page_url,
            cmg,
            video_state: CmgVideoState::default(),
            mux_state: TsMuxState::default(),
            processed: HashMap::new(),
            last_processed_sequence: None,
            reset_count: 0,
        }));
        let mut state = self.state.lock().await;
        Ok(state
            .runtimes
            .entry(livepid.to_string())
            .or_insert_with(|| runtime.clone())
            .clone())
    }

    async fn process_segment(
        &self,
        runtime: Arc<Mutex<ChannelRuntime>>,
        segment: SegmentRef,
    ) -> Result<Bytes> {
        let mut runtime = runtime.lock().await;
        let sequence = segment.sequence;
        if let Some(cached) = runtime.processed.get(&sequence) {
            return Ok(cached.bytes.clone());
        }

        match runtime.last_processed_sequence {
            None => {
                for predecessor in self
                    .contiguous_predecessors(&segment.livepid, sequence)
                    .await
                {
                    if runtime.processed.contains_key(&predecessor.sequence) {
                        continue;
                    }
                    self.process_segment_payload_locked(&mut runtime, predecessor)
                        .await?;
                }
            }
            Some(last) if sequence < last => {
                return Err(anyhow!(
                    "segment {sequence} is older than runtime sequence {last} and was not cached"
                ));
            }
            Some(last) if sequence > last + 1 => {
                let missing = self
                    .missing_predecessors(&segment.livepid, last + 1, sequence)
                    .await;
                if missing.len() != (sequence - last - 1) as usize {
                    reset_runtime_locked(&mut runtime).with_context(|| {
                        format!("reset CMG runtime after sequence gap {last}->{sequence}")
                    })?;
                    for predecessor in self
                        .contiguous_predecessors(&segment.livepid, sequence)
                        .await
                    {
                        self.process_segment_payload_locked(&mut runtime, predecessor)
                            .await?;
                    }
                } else {
                    for predecessor in missing {
                        self.process_segment_payload_locked(&mut runtime, predecessor)
                            .await?;
                    }
                }
            }
            _ => {}
        }

        let processed = self
            .process_segment_payload_locked(&mut runtime, segment)
            .await?;
        Ok(processed.bytes)
    }

    async fn process_segment_payload_locked(
        &self,
        runtime: &mut ChannelRuntime,
        segment: SegmentRef,
    ) -> Result<ProcessedSegment> {
        if let Some(cached) = runtime.processed.get(&segment.sequence) {
            return Ok(cached.clone());
        }
        let response = self
            .http
            .get(&segment.url)
            .header("referer", format!("{ACTIVE_URL}/"))
            .header("user-agent", USER_AGENT)
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            let bytes = response.bytes().await?;
            return Err(anyhow!(
                "upstream segment failed status={}: {}",
                status.as_u16(),
                String::from_utf8_lossy(&bytes[..bytes.len().min(300)])
            ));
        }
        let input = response.bytes().await?;
        let (output, stats) = decrypt_and_remux_ts(
            &mut runtime.cmg,
            &mut runtime.video_state,
            &mut runtime.mux_state,
            &runtime.media_tag_id,
            ACTIVE_URL,
            &input,
        )
        .with_context(|| {
            format!(
                "decrypt TS segment sequence={} url={}",
                segment.sequence, segment.url
            )
        })?;
        dump_segment_if_enabled(
            SegmentDumpMeta {
                ch: &segment.ch,
                livepid: &segment.livepid,
                sequence: segment.sequence,
                id: &segment.id,
                url: &segment.url,
                media_tag_id: &runtime.media_tag_id,
                vmp_tag: runtime.cmg.vmp_tag(),
                reset_count: runtime.reset_count,
                input_bytes: input.len(),
                output_bytes: output.len(),
                stats,
            },
            &input,
            &output,
        )?;
        if std::env::var_os("IPTV_RUST_TRACE_SEGMENTS").is_some() {
            info!(
                ch = %segment.ch,
                livepid = %segment.livepid,
                sequence = segment.sequence,
                input_bytes = input.len(),
                output_bytes = output.len(),
                media_tag_id = %runtime.media_tag_id,
                vmp_tag = %runtime.cmg.vmp_tag(),
                video_pid = stats.input_video_pid,
                audio_pid = stats.input_audio_pid,
                video_samples = stats.video_sample_count,
                audio_samples = stats.audio_sample_count,
                nal_count = stats.nal_count,
                decoded_nals = stats.decoded_nals,
                changed_nals = stats.changed_nals,
                changed_bytes = stats.changed_bytes,
                shorter_nals = stats.shorter_nals,
                sps_side_effects = stats.sps_side_effects,
                reset_count = runtime.reset_count,
                "decrypted and remuxed TS segment"
            );
        }
        let processed = ProcessedSegment {
            bytes: Bytes::from(output),
            stats,
        };
        runtime
            .processed
            .insert(segment.sequence, processed.clone());
        trim_map(&mut runtime.processed, MEDIA_HISTORY_MAX_SEGMENTS);
        runtime.last_processed_sequence = Some(
            runtime
                .last_processed_sequence
                .map(|last| last.max(segment.sequence))
                .unwrap_or(segment.sequence),
        );
        Ok(processed)
    }

    async fn fetch_media_playlist(&self, playback_url: &str) -> Result<MediaPlaylist> {
        let mut url = playback_url.to_string();
        for depth in 0..3 {
            let text = self
                .http
                .get(&url)
                .header("referer", format!("{ACTIVE_URL}/"))
                .header("user-agent", USER_AGENT)
                .send()
                .await
                .with_context(|| format!("send upstream m3u8 request depth={depth} url={url}"))?
                .error_for_status()
                .with_context(|| format!("upstream m3u8 status error depth={depth} url={url}"))?
                .text()
                .await?;
            let parsed = parse_m3u8(&text, &url)?;
            if !parsed.segments.is_empty() {
                return Ok(MediaPlaylist {
                    url,
                    media_sequence: parsed.media_sequence,
                    segments: parsed.segments,
                });
            }
            let Some(next) = parsed.playlists.first() else {
                return Err(anyhow!("m3u8 has no segments or child playlists: {url}"));
            };
            url = next.clone();
        }
        Err(anyhow!("m3u8 recursion limit exceeded for {playback_url}"))
    }

    async fn update_history(&self, channel: &Channel, playlist: &MediaPlaylist) -> Vec<SegmentRef> {
        let mut state = self.state.lock().await;
        let history = state.history.entry(channel.livepid.clone()).or_default();
        for segment in &playlist.segments {
            if history.iter().any(|item| item.sequence == segment.sequence) {
                continue;
            }
            history.push(SegmentRef {
                id: segment_sequence_id(&channel.livepid, segment.sequence),
                ch: channel.ch.clone(),
                livepid: channel.livepid.clone(),
                url: segment.url.clone(),
                duration: segment.duration,
                sequence: segment.sequence,
            });
        }
        history.sort_by_key(|segment| segment.sequence);
        if history.len() > MEDIA_HISTORY_MAX_SEGMENTS {
            let keep_from = history.len() - MEDIA_HISTORY_MAX_SEGMENTS;
            history.drain(0..keep_from);
        }
        history.clone()
    }

    async fn cached_history(&self, livepid: &str) -> Vec<SegmentRef> {
        self.state
            .lock()
            .await
            .history
            .get(livepid)
            .cloned()
            .unwrap_or_default()
    }

    async fn contiguous_predecessors(
        &self,
        livepid: &str,
        target_sequence: i64,
    ) -> Vec<SegmentRef> {
        let state = self.state.lock().await;
        let Some(history) = state.history.get(livepid) else {
            return Vec::new();
        };
        let mut result = Vec::new();
        let mut expected = target_sequence - 1;
        for segment in history.iter().rev() {
            if segment.sequence >= target_sequence {
                continue;
            }
            if segment.sequence == expected {
                result.push(segment.clone());
                expected -= 1;
                if result.len() >= MEDIA_PLAYLIST_WINDOW_SEGMENTS.saturating_sub(1) {
                    break;
                }
            } else if segment.sequence < expected {
                break;
            }
        }
        result.reverse();
        result
    }

    async fn missing_predecessors(
        &self,
        livepid: &str,
        start_sequence: i64,
        target_sequence: i64,
    ) -> Vec<SegmentRef> {
        let state = self.state.lock().await;
        let Some(history) = state.history.get(livepid) else {
            return Vec::new();
        };
        let by_sequence: HashMap<i64, SegmentRef> = history
            .iter()
            .cloned()
            .map(|segment| (segment.sequence, segment))
            .collect();
        let mut result = Vec::new();
        for sequence in start_sequence..target_sequence {
            let Some(segment) = by_sequence.get(&sequence).cloned() else {
                return result;
            };
            result.push(segment);
        }
        result
    }
}

fn reset_runtime_locked(runtime: &mut ChannelRuntime) -> Result<()> {
    let media_tag_id = new_media_tag_id();
    let mut cmg = CmgRuntime::load_for_page(&runtime.page_url)?;
    cmg.prime(&media_tag_id)?;
    runtime.media_tag_id = media_tag_id;
    runtime.cmg = cmg;
    runtime.video_state = CmgVideoState::default();
    runtime.mux_state = TsMuxState::default();
    runtime.processed.clear();
    runtime.last_processed_sequence = None;
    runtime.reset_count += 1;
    Ok(())
}

fn parse_m3u8(text: &str, base_url: &str) -> Result<ParsedM3u8> {
    let base = Url::parse(base_url).with_context(|| format!("parse m3u8 base url: {base_url}"))?;
    let mut media_sequence = 0i64;
    let mut pending_duration = None;
    let mut segments = Vec::new();
    let mut playlists = Vec::new();
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if let Some(value) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            media_sequence = value.trim().parse::<i64>().unwrap_or(0);
        } else if let Some(value) = line.strip_prefix("#EXTINF:") {
            pending_duration = value
                .split(',')
                .next()
                .and_then(|value| value.trim().parse::<f64>().ok());
        } else if line.starts_with('#') {
            continue;
        } else {
            let url = base
                .join(line)
                .with_context(|| format!("resolve m3u8 line against {base_url}: {line}"))?
                .to_string();
            if line.contains(".m3u8") {
                playlists.push(url);
            } else {
                segments.push(ParsedSegment {
                    url,
                    duration: pending_duration.unwrap_or(5.0),
                    sequence: media_sequence + segments.len() as i64,
                });
            }
            pending_duration = None;
        }
    }
    Ok(ParsedM3u8 {
        media_sequence,
        segments,
        playlists,
    })
}

fn error_chain(error: &anyhow::Error) -> String {
    error
        .chain()
        .map(|cause| cause.to_string())
        .collect::<Vec<_>>()
        .join("; caused by: ")
}

fn contiguous_segment_suffix(segments: &[SegmentRef], max_count: usize) -> Vec<SegmentRef> {
    let ordered: Vec<SegmentRef> = segments
        .iter()
        .cloned()
        .rev()
        .take(max_count)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let mut suffix: Vec<SegmentRef> = Vec::new();
    for segment in ordered.into_iter().rev() {
        if suffix
            .first()
            .map(|first| segment.sequence + 1 == first.sequence)
            .unwrap_or(true)
        {
            suffix.insert(0, segment);
        } else {
            break;
        }
    }
    suffix
}

fn playable_segment_window(segments: &[SegmentRef]) -> Vec<SegmentRef> {
    let segments = drop_live_edge_segments(segments, MEDIA_LIVE_EDGE_HOLDBACK_SEGMENTS);
    contiguous_segment_suffix(&segments, MEDIA_PLAYLIST_WINDOW_SEGMENTS)
}

fn drop_live_edge_segments(segments: &[SegmentRef], holdback: usize) -> Vec<SegmentRef> {
    let keep_len = segments.len().saturating_sub(holdback);
    segments.iter().take(keep_len).cloned().collect()
}

fn segment_sequence_id(livepid: &str, sequence: i64) -> String {
    let digest = Sha1::digest(format!("{livepid}:{sequence}").as_bytes());
    hex::encode(digest)[..20].to_string()
}

fn trim_map<K, V>(map: &mut HashMap<K, V>, max_size: usize)
where
    K: Eq + std::hash::Hash + Clone,
{
    while map.len() > max_size {
        let Some(key) = map.keys().next().cloned() else {
            break;
        };
        map.remove(&key);
    }
}

fn new_media_tag_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis();
    now.to_string()
}

fn dump_segment_if_enabled(meta: SegmentDumpMeta<'_>, input: &[u8], output: &[u8]) -> Result<()> {
    let Some(dir) = std::env::var_os("IPTV_RUST_DUMP_SEGMENTS_DIR") else {
        return Ok(());
    };
    let dir = std::path::Path::new(&dir);
    std::fs::create_dir_all(dir)?;
    let base = format!(
        "{}-{}-{}",
        sanitize_filename(meta.ch),
        meta.sequence,
        meta.id
    );
    std::fs::write(dir.join(format!("{base}-raw.ts")), input)?;
    std::fs::write(dir.join(format!("{base}-rust.ts")), output)?;
    std::fs::write(
        dir.join(format!("{base}.json")),
        serde_json::to_vec_pretty(&meta)?,
    )?;
    Ok(())
}

fn sanitize_filename(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_media_playlist_segments() {
        let parsed = parse_m3u8(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:10\n#EXTINF:2.000,\na.ts\n#EXTINF:3.500,\nb.ts\n",
            "https://example.com/live/index.m3u8?x=1",
        )
        .unwrap();
        assert_eq!(parsed.media_sequence, 10);
        assert_eq!(parsed.segments.len(), 2);
        assert_eq!(parsed.segments[0].sequence, 10);
        assert_eq!(parsed.segments[1].sequence, 11);
        assert_eq!(parsed.segments[0].url, "https://example.com/live/a.ts");
    }
}
