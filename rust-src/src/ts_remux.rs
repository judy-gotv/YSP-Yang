use std::collections::HashMap;

use anyhow::{anyhow, Result};
use serde::Serialize;

use crate::cmg::CmgRuntime;

const OUT_VIDEO_PID: u16 = 0x0100;
const OUT_AUDIO_PID: u16 = 0x0101;
const OUT_PMT_PID: u16 = 0x1000;

const AAC_SAMPLE_RATES: [u32; 13] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000,
    7_350,
];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct RemuxStats {
    pub input_video_pid: u16,
    pub input_audio_pid: u16,
    pub video_pes_count: usize,
    pub audio_pes_count: usize,
    pub video_sample_count: usize,
    pub audio_sample_count: usize,
    pub nal_count: usize,
    pub decoded_nals: usize,
    pub changed_nals: usize,
    pub changed_bytes: usize,
    pub shorter_nals: usize,
    pub sps_side_effects: usize,
    pub output_bytes: usize,
}

#[derive(Debug, Default)]
pub struct CmgVideoState {
    live_sps_enabled: bool,
    last_sps: Option<Vec<u8>>,
    last_pps: Option<Vec<u8>>,
}

#[derive(Debug, Default)]
pub struct TsMuxState {
    continuity: HashMap<u16, u8>,
}

#[derive(Debug, Clone)]
struct Packet {
    offset: usize,
    pid: u16,
    pusi: bool,
    payload_offset: usize,
    payload_end: usize,
}

#[derive(Debug, Clone)]
struct StreamInfo {
    pid: u16,
    stream_type: u8,
}

#[derive(Debug, Clone)]
struct Pes {
    stream_id: u8,
    pts: Option<i64>,
    dts: Option<i64>,
    payload: Vec<u8>,
}

#[derive(Debug, Clone)]
struct Nal {
    nal_type: u8,
    data: Vec<u8>,
}

#[derive(Debug, Clone)]
struct MediaEvent {
    kind: EventKind,
    dts90: i64,
    pts90: i64,
    data: Vec<u8>,
    keyframe: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventKind {
    Video,
    Audio,
}

pub fn decrypt_and_remux_ts(
    runtime: &mut CmgRuntime,
    video_state: &mut CmgVideoState,
    mux_state: &mut TsMuxState,
    media_tag_id: &str,
    active_url: &str,
    input: &[u8],
) -> Result<(Vec<u8>, RemuxStats)> {
    let packets = parse_ts_packets(input)?;
    let pmt_pid = find_pmt_pid(input, &packets)?;
    let streams = find_streams(input, &packets, pmt_pid)?;
    let video_pid = streams
        .iter()
        .find(|stream| stream.stream_type == 0x1b)
        .map(|stream| stream.pid)
        .ok_or_else(|| anyhow!("PMT did not expose H264 video PID"))?;
    let audio_pid = streams
        .iter()
        .find(|stream| stream.stream_type == 0x0f)
        .map(|stream| stream.pid)
        .ok_or_else(|| anyhow!("PMT did not expose AAC audio PID"))?;

    let video_pes = collect_pes(input, &packets, video_pid);
    let audio_pes = collect_pes(input, &packets, audio_pid);
    let mut stats = RemuxStats {
        input_video_pid: video_pid,
        input_audio_pid: audio_pid,
        video_pes_count: video_pes.len(),
        audio_pes_count: audio_pes.len(),
        ..RemuxStats::default()
    };

    let mut events = Vec::with_capacity(video_pes.len() + audio_pes.len() * 2);
    for pes in &video_pes {
        if let Some(event) = decrypt_video_pes(
            runtime,
            video_state,
            media_tag_id,
            active_url,
            pes,
            &mut stats,
        )? {
            events.push(event);
        }
    }
    for pes in &audio_pes {
        append_audio_events(pes, &mut events, &mut stats);
    }
    if !events.iter().any(|event| event.kind == EventKind::Video) {
        return Err(anyhow!("no video samples after TS demux"));
    }
    if !events.iter().any(|event| event.kind == EventKind::Audio) {
        return Err(anyhow!("no audio samples after TS demux"));
    }

    let output = mux_events_to_ts(&mut events, mux_state);
    stats.output_bytes = output.len();
    Ok((output, stats))
}

fn decrypt_video_pes(
    runtime: &mut CmgRuntime,
    video_state: &mut CmgVideoState,
    media_tag_id: &str,
    active_url: &str,
    pes: &Pes,
    stats: &mut RemuxStats,
) -> Result<Option<MediaEvent>> {
    let Some(pts) = pes.pts else {
        return Ok(None);
    };
    let dts = pes.dts.unwrap_or(pts);
    let nals = find_annex_b_nals(&pes.payload);
    if nals.is_empty() {
        return Ok(None);
    }

    let mut out_nals = Vec::with_capacity(nals.len() + 2);
    let mut keyframe = false;
    for mut nal in nals {
        stats.nal_count += 1;
        runtime.update(media_tag_id)?;
        match nal.nal_type {
            1 | 5 => {
                if video_state.live_sps_enabled {
                    stats.decoded_nals += 1;
                    let decoded = runtime.module_dec_live(media_tag_id, &nal.data, active_url)?;
                    let diff = count_byte_diff(&nal.data, &decoded);
                    if diff > 0 {
                        stats.changed_nals += 1;
                        stats.changed_bytes += diff;
                    }
                    if decoded.len() < nal.data.len() {
                        stats.shorter_nals += 1;
                    }
                    if decoded.len() > nal.data.len() {
                        return Err(anyhow!(
                            "CMG output grew from {} to {} bytes for H264 NAL type {}",
                            nal.data.len(),
                            decoded.len(),
                            nal.nal_type
                        ));
                    }
                    nal.data = decoded;
                }
                if nal.nal_type == 5 {
                    keyframe = true;
                }
                out_nals.push(nal);
            }
            7 => {
                if nal.data.len() > 2 {
                    if !video_state.live_sps_enabled {
                        let marker = nal.data[2] & 0x03;
                        video_state.live_sps_enabled = marker == 1 || marker == 2;
                    }
                    let _ = runtime.module_dec_live(media_tag_id, &nal.data, active_url)?;
                    nal.data[2] = 0;
                    stats.sps_side_effects += 1;
                }
                video_state.last_sps = Some(nal.data.clone());
                out_nals.push(nal);
            }
            8 => {
                video_state.last_pps = Some(nal.data.clone());
                out_nals.push(nal);
            }
            9 => {
                out_nals.push(nal);
            }
            _ => out_nals.push(nal),
        }
    }

    if out_nals.is_empty() {
        return Ok(None);
    }
    let mut data = Vec::new();
    data.extend_from_slice(&[0, 0, 0, 1, 0x09, 0xf0]);
    if keyframe {
        if let Some(sps) = &video_state.last_sps {
            push_annex_b(&mut data, sps);
        }
        if let Some(pps) = &video_state.last_pps {
            push_annex_b(&mut data, pps);
        }
    }
    for nal in out_nals {
        push_annex_b(&mut data, &nal.data);
    }
    stats.video_sample_count += 1;
    Ok(Some(MediaEvent {
        kind: EventKind::Video,
        dts90: dts,
        pts90: pts,
        data,
        keyframe,
    }))
}

fn append_audio_events(pes: &Pes, events: &mut Vec<MediaEvent>, stats: &mut RemuxStats) {
    let Some(base_pts) = pes.pts else {
        return;
    };
    let mut offset = 0usize;
    let mut index = 0usize;
    while offset + 7 <= pes.payload.len() {
        if pes.payload[offset] != 0xff || (pes.payload[offset + 1] & 0xf0) != 0xf0 {
            offset += 1;
            continue;
        }
        let sample_rate_index = (pes.payload[offset + 2] >> 2) & 0x0f;
        let sample_rate = AAC_SAMPLE_RATES
            .get(sample_rate_index as usize)
            .copied()
            .unwrap_or(44_100);
        let frame_length = (((pes.payload[offset + 3] & 0x03) as usize) << 11)
            | ((pes.payload[offset + 4] as usize) << 3)
            | ((pes.payload[offset + 5] as usize) >> 5);
        if frame_length < 7 || offset + frame_length > pes.payload.len() {
            break;
        }
        let delta = ((index as i64) * 1024 * 90_000) / sample_rate as i64;
        events.push(MediaEvent {
            kind: EventKind::Audio,
            dts90: base_pts + delta,
            pts90: base_pts + delta,
            data: pes.payload[offset..offset + frame_length].to_vec(),
            keyframe: false,
        });
        stats.audio_sample_count += 1;
        index += 1;
        offset += frame_length;
    }
}

fn mux_events_to_ts(events: &mut Vec<MediaEvent>, state: &mut TsMuxState) -> Vec<u8> {
    let mut packets = Vec::new();
    packets.push(packetize_psi(0x0000, &make_pat_section(), state));
    packets.push(packetize_psi(OUT_PMT_PID, &make_pmt_section(), state));

    let first_video_index = events
        .iter()
        .position(|event| event.kind == EventKind::Video);
    if let Some(index) = first_video_index {
        let first_video = events.remove(index);
        packets.extend(packetize_event(&first_video, state));
    }
    events.sort_by(|a, b| {
        a.dts90.cmp(&b.dts90).then_with(|| match (a.kind, b.kind) {
            (EventKind::Video, EventKind::Audio) => std::cmp::Ordering::Less,
            (EventKind::Audio, EventKind::Video) => std::cmp::Ordering::Greater,
            _ => std::cmp::Ordering::Equal,
        })
    });
    for event in events {
        packets.extend(packetize_event(event, state));
    }
    packets.concat()
}

fn packetize_event(event: &MediaEvent, state: &mut TsMuxState) -> Vec<Vec<u8>> {
    match event.kind {
        EventKind::Video => packetize_pes(
            OUT_VIDEO_PID,
            0xe0,
            &event.data,
            event.pts90,
            event.dts90,
            state,
            Some((event.dts90, event.keyframe)),
        ),
        EventKind::Audio => packetize_pes(
            OUT_AUDIO_PID,
            0xc0,
            &event.data,
            event.pts90,
            event.dts90,
            state,
            None,
        ),
    }
}

fn packetize_pes(
    pid: u16,
    stream_id: u8,
    payload: &[u8],
    pts: i64,
    dts: i64,
    state: &mut TsMuxState,
    pcr: Option<(i64, bool)>,
) -> Vec<Vec<u8>> {
    let pes = make_pes(stream_id, payload, pts, dts);
    let mut packets = Vec::new();
    let mut offset = 0usize;
    let mut first = true;
    while offset < pes.len() {
        let mut packet = vec![0xff; 188];
        let with_pcr = first && pcr.is_some();
        let remaining = pes.len() - offset;
        let mut payload_capacity = 184usize;
        let mut afc = 1u8;
        let mut adaptation_length = 0usize;
        let mut adaptation_flags = 0u8;
        if with_pcr {
            afc = 3;
            adaptation_flags = 0x10
                | if pcr.map(|(_, random)| random).unwrap_or(false) {
                    0x40
                } else {
                    0
                };
            payload_capacity = 176;
            adaptation_length = if remaining < payload_capacity {
                7 + (payload_capacity - remaining)
            } else {
                7
            };
        } else if remaining < 184 {
            afc = 3;
            adaptation_length = 183 - remaining;
            payload_capacity = remaining;
        }
        let payload_len = remaining.min(payload_capacity);
        packet[0] = 0x47;
        packet[1] = if first { 0x40 } else { 0x00 } | ((pid >> 8) as u8 & 0x1f);
        packet[2] = pid as u8;
        packet[3] = (afc << 4) | next_continuity(state, pid);
        let mut cursor = 4usize;
        if afc == 3 {
            packet[cursor] = adaptation_length as u8;
            cursor += 1;
            if adaptation_length > 0 {
                packet[cursor] = adaptation_flags;
                cursor += 1;
                if let Some((pcr_value, _)) = pcr.filter(|_| with_pcr) {
                    let bytes = pcr_bytes(pcr_value);
                    packet[cursor..cursor + 6].copy_from_slice(&bytes);
                    cursor += 6;
                }
                while cursor < 188 - payload_len {
                    packet[cursor] = 0xff;
                    cursor += 1;
                }
            }
        }
        packet[cursor..cursor + payload_len].copy_from_slice(&pes[offset..offset + payload_len]);
        offset += payload_len;
        packets.push(packet);
        first = false;
    }
    packets
}

fn make_pes(stream_id: u8, payload: &[u8], pts: i64, dts: i64) -> Vec<u8> {
    let use_dts = pts != dts;
    let mut timestamp = Vec::with_capacity(if use_dts { 10 } else { 5 });
    if use_dts {
        timestamp.extend_from_slice(&encode_timestamp(0x03, pts));
        timestamp.extend_from_slice(&encode_timestamp(0x01, dts));
    } else {
        timestamp.extend_from_slice(&encode_timestamp(0x02, pts));
    }
    let mut out = Vec::with_capacity(9 + timestamp.len() + payload.len());
    out.extend_from_slice(&[
        0,
        0,
        1,
        stream_id,
        0,
        0,
        0x80,
        if use_dts { 0xc0 } else { 0x80 },
        timestamp.len() as u8,
    ]);
    if stream_id != 0xe0 {
        let length = (payload.len() + 3 + timestamp.len()).min(u16::MAX as usize) as u16;
        out[4] = (length >> 8) as u8;
        out[5] = length as u8;
    }
    out.extend_from_slice(&timestamp);
    out.extend_from_slice(payload);
    out
}

fn packetize_psi(pid: u16, section: &[u8], state: &mut TsMuxState) -> Vec<u8> {
    let mut packet = vec![0xff; 188];
    packet[0] = 0x47;
    packet[1] = 0x40 | ((pid >> 8) as u8 & 0x1f);
    packet[2] = pid as u8;
    packet[3] = 0x10 | next_continuity(state, pid);
    packet[4] = 0x00;
    let end = 5 + section.len().min(183);
    packet[5..end].copy_from_slice(&section[..end - 5]);
    packet
}

fn next_continuity(state: &mut TsMuxState, pid: u16) -> u8 {
    let current = *state.continuity.get(&pid).unwrap_or(&0) & 0x0f;
    state.continuity.insert(pid, (current + 1) & 0x0f);
    current
}

fn make_pat_section() -> Vec<u8> {
    let body = vec![
        0x00,
        0x01,
        0xc1,
        0x00,
        0x00,
        0x00,
        0x01,
        0xe0 | ((OUT_PMT_PID >> 8) as u8 & 0x1f),
        OUT_PMT_PID as u8,
    ];
    let section_length = body.len() + 4;
    section_with_crc(
        &[
            0x00,
            0xb0 | ((section_length >> 8) as u8 & 0x0f),
            section_length as u8,
        ],
        &body,
    )
}

fn make_pmt_section() -> Vec<u8> {
    let stream_info = vec![
        0x1b,
        0xe0 | ((OUT_VIDEO_PID >> 8) as u8 & 0x1f),
        OUT_VIDEO_PID as u8,
        0xf0,
        0x00,
        0x0f,
        0xe0 | ((OUT_AUDIO_PID >> 8) as u8 & 0x1f),
        OUT_AUDIO_PID as u8,
        0xf0,
        0x00,
    ];
    let mut body = vec![
        0x00,
        0x01,
        0xc1,
        0x00,
        0x00,
        0xe0 | ((OUT_VIDEO_PID >> 8) as u8 & 0x1f),
        OUT_VIDEO_PID as u8,
        0xf0,
        0x00,
    ];
    body.extend_from_slice(&stream_info);
    let section_length = body.len() + 4;
    section_with_crc(
        &[
            0x02,
            0xb0 | ((section_length >> 8) as u8 & 0x0f),
            section_length as u8,
        ],
        &body,
    )
}

fn section_with_crc(header: &[u8], body: &[u8]) -> Vec<u8> {
    let mut section = Vec::with_capacity(header.len() + body.len() + 4);
    section.extend_from_slice(header);
    section.extend_from_slice(body);
    let crc = crc32_mpeg(&section);
    section.extend_from_slice(&crc.to_be_bytes());
    section
}

fn crc32_mpeg(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &byte in bytes {
        crc ^= (byte as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04c1_1db7
            } else {
                crc << 1
            };
        }
    }
    crc
}

fn encode_timestamp(prefix: u8, timestamp: i64) -> [u8; 5] {
    let value = (timestamp.max(0) as u64) & ((1u64 << 33) - 1);
    [
        (prefix << 4) | ((((value >> 30) & 0x07) as u8) << 1) | 1,
        ((value >> 22) & 0xff) as u8,
        ((((value >> 15) & 0x7f) as u8) << 1) | 1,
        ((value >> 7) & 0xff) as u8,
        (((value & 0x7f) as u8) << 1) | 1,
    ]
}

fn pcr_bytes(timestamp: i64) -> [u8; 6] {
    let base = (timestamp.max(0) as u64) & ((1u64 << 33) - 1);
    [
        ((base >> 25) & 0xff) as u8,
        ((base >> 17) & 0xff) as u8,
        ((base >> 9) & 0xff) as u8,
        ((base >> 1) & 0xff) as u8,
        (((base & 1) as u8) << 7) | 0x7e,
        0x00,
    ]
}

fn parse_ts_packets(input: &[u8]) -> Result<Vec<Packet>> {
    if input.len() % 188 != 0 {
        return Err(anyhow!(
            "TS segment is not 188-byte aligned: {}",
            input.len()
        ));
    }
    let mut packets = Vec::with_capacity(input.len() / 188);
    for offset in (0..input.len()).step_by(188) {
        if input[offset] != 0x47 {
            return Err(anyhow!("bad TS sync byte at offset {offset}"));
        }
        let pusi = input[offset + 1] & 0x40 != 0;
        let pid = (((input[offset + 1] & 0x1f) as u16) << 8) | input[offset + 2] as u16;
        let afc = (input[offset + 3] >> 4) & 0x03;
        let mut payload_offset = offset + 4;
        if afc == 2 || afc == 3 {
            payload_offset += 1 + input[offset + 4] as usize;
        }
        let has_payload = afc == 1 || afc == 3;
        if !has_payload || payload_offset >= offset + 188 {
            payload_offset = offset + 188;
        }
        packets.push(Packet {
            offset,
            pid,
            pusi,
            payload_offset,
            payload_end: offset + 188,
        });
    }
    Ok(packets)
}

fn find_pmt_pid(input: &[u8], packets: &[Packet]) -> Result<u16> {
    for packet in packets {
        if packet.pid == 0 && packet.pusi && packet.payload_offset < packet.payload_end {
            if let Some(pid) = parse_pat(&input[packet.payload_offset..packet.payload_end]) {
                return Ok(pid);
            }
        }
    }
    Err(anyhow!("PAT did not expose PMT PID"))
}

fn find_streams(input: &[u8], packets: &[Packet], pmt_pid: u16) -> Result<Vec<StreamInfo>> {
    for packet in packets {
        if packet.pid == pmt_pid && packet.pusi && packet.payload_offset < packet.payload_end {
            let streams = parse_pmt(&input[packet.payload_offset..packet.payload_end]);
            if !streams.is_empty() {
                return Ok(streams);
            }
        }
    }
    Err(anyhow!("PMT did not expose elementary streams"))
}

fn parse_pat(payload: &[u8]) -> Option<u16> {
    if payload.len() < 8 {
        return None;
    }
    let pointer = payload[0] as usize;
    let mut offset = 1 + pointer;
    if payload.get(offset).copied()? != 0x00 || offset + 8 > payload.len() {
        return None;
    }
    let section_length =
        (((payload[offset + 1] & 0x0f) as usize) << 8) | payload[offset + 2] as usize;
    let end = offset + 3 + section_length.checked_sub(4)?;
    offset += 8;
    while offset + 4 <= end && offset + 4 <= payload.len() {
        let program = ((payload[offset] as u16) << 8) | payload[offset + 1] as u16;
        let pid = (((payload[offset + 2] & 0x1f) as u16) << 8) | payload[offset + 3] as u16;
        if program != 0 {
            return Some(pid);
        }
        offset += 4;
    }
    None
}

fn parse_pmt(payload: &[u8]) -> Vec<StreamInfo> {
    let Some(pointer) = payload.first().copied() else {
        return Vec::new();
    };
    let mut offset = 1 + pointer as usize;
    if payload.get(offset).copied() != Some(0x02) || offset + 12 > payload.len() {
        return Vec::new();
    }
    let section_length =
        (((payload[offset + 1] & 0x0f) as usize) << 8) | payload[offset + 2] as usize;
    let program_info_length =
        (((payload[offset + 10] & 0x0f) as usize) << 8) | payload[offset + 11] as usize;
    let Some(end) = offset.checked_add(3 + section_length.saturating_sub(4)) else {
        return Vec::new();
    };
    offset += 12 + program_info_length;
    let mut streams = Vec::new();
    while offset + 5 <= end && offset + 5 <= payload.len() {
        let stream_type = payload[offset];
        let pid = (((payload[offset + 1] & 0x1f) as u16) << 8) | payload[offset + 2] as u16;
        let info_length =
            (((payload[offset + 3] & 0x0f) as usize) << 8) | payload[offset + 4] as usize;
        streams.push(StreamInfo { pid, stream_type });
        offset += 5 + info_length;
    }
    streams
}

fn collect_pes(input: &[u8], packets: &[Packet], pid: u16) -> Vec<Pes> {
    let mut result = Vec::new();
    let mut current = Vec::new();
    for packet in packets.iter().filter(|packet| packet.pid == pid) {
        if packet.payload_offset >= packet.payload_end {
            continue;
        }
        if packet.pusi && !current.is_empty() {
            if let Some(pes) = parse_pes(&current) {
                result.push(pes);
            }
            current.clear();
        }
        current.extend_from_slice(&input[packet.payload_offset..packet.payload_end]);
    }
    if !current.is_empty() {
        if let Some(pes) = parse_pes(&current) {
            result.push(pes);
        }
    }
    result
}

fn parse_pes(bytes: &[u8]) -> Option<Pes> {
    if bytes.len() < 9 || bytes[0] != 0 || bytes[1] != 0 || bytes[2] != 1 {
        return None;
    }
    let stream_id = bytes[3];
    let flags = bytes[7];
    let header_len = bytes[8] as usize;
    let payload_start = 9 + header_len;
    if payload_start > bytes.len() {
        return None;
    }
    let pts = if flags & 0x80 != 0 && bytes.len() >= 14 {
        Some(parse_timestamp(&bytes[9..14]))
    } else {
        None
    };
    let dts = if flags & 0x40 != 0 && bytes.len() >= 19 {
        Some(parse_timestamp(&bytes[14..19]))
    } else {
        None
    };
    Some(Pes {
        stream_id,
        pts,
        dts,
        payload: bytes[payload_start..].to_vec(),
    })
}

fn parse_timestamp(bytes: &[u8]) -> i64 {
    if bytes.len() < 5 {
        return 0;
    }
    ((((bytes[0] >> 1) & 0x07) as i64) << 30)
        | ((bytes[1] as i64) << 22)
        | ((((bytes[2] >> 1) & 0x7f) as i64) << 15)
        | ((bytes[3] as i64) << 7)
        | (((bytes[4] >> 1) & 0x7f) as i64)
}

fn find_annex_b_nals(payload: &[u8]) -> Vec<Nal> {
    let mut starts = Vec::new();
    let mut index = 0usize;
    while index + 3 < payload.len() {
        if payload[index] == 0 && payload[index + 1] == 0 && payload[index + 2] == 1 {
            starts.push((index, index + 3));
            index += 3;
        } else if index + 4 < payload.len()
            && payload[index] == 0
            && payload[index + 1] == 0
            && payload[index + 2] == 0
            && payload[index + 3] == 1
        {
            starts.push((index, index + 4));
            index += 4;
        } else {
            index += 1;
        }
    }

    let mut nals = Vec::new();
    for idx in 0..starts.len() {
        let next_prefix = starts
            .get(idx + 1)
            .map(|(prefix, _)| *prefix)
            .unwrap_or(payload.len());
        let nal_start = starts[idx].1;
        if nal_start < next_prefix {
            let data = payload[nal_start..next_prefix].to_vec();
            if !data.is_empty() {
                nals.push(Nal {
                    nal_type: data[0] & 0x1f,
                    data,
                });
            }
        }
    }
    nals
}

fn push_annex_b(out: &mut Vec<u8>, nal: &[u8]) {
    out.extend_from_slice(&[0, 0, 0, 1]);
    out.extend_from_slice(nal);
}

fn count_byte_diff(left: &[u8], right: &[u8]) -> usize {
    let mut count = left.len().abs_diff(right.len());
    let min_len = left.len().min(right.len());
    for index in 0..min_len {
        if left[index] != right[index] {
            count += 1;
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_TS: &[u8] = include_bytes!("../../evidence/live-sample-1762263159.ts");

    #[test]
    fn remuxes_sample_ts() {
        let page_url = std::env::var("RUST_REMUX_PAGE_URL")
            .unwrap_or_else(|_| "https://www.yangshipin.cn/tv/home?pid=600001859".to_string());
        let mut runtime = CmgRuntime::load_for_page(&page_url).unwrap();
        let media_tag_id = "1778267239522";
        runtime.prime(media_tag_id).unwrap();
        let mut video_state = CmgVideoState::default();
        let mut mux_state = TsMuxState::default();
        let (output, stats) = decrypt_and_remux_ts(
            &mut runtime,
            &mut video_state,
            &mut mux_state,
            media_tag_id,
            "https://www.yangshipin.cn",
            SAMPLE_TS,
        )
        .unwrap();
        assert!(!output.is_empty());
        assert_eq!(output.len() % 188, 0);
        assert!(stats.video_sample_count > 0);
        assert!(stats.audio_sample_count > 0);
        if std::env::var_os("RUST_WRITE_TS_FIXTURE").is_some() {
            let path = std::path::Path::new("../run/rust-verify/rust-remux.ts");
            std::fs::write(path, output).unwrap();
        }
    }

    #[test]
    fn remuxes_dump_batch_when_requested() {
        let Some(input_dir) = std::env::var_os("RUST_REMUX_BATCH_INPUT_DIR") else {
            return;
        };
        let output_dir = std::env::var_os("RUST_REMUX_BATCH_OUTPUT_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("../run/rust-remux-batch"));
        std::fs::create_dir_all(&output_dir).unwrap();
        let mut inputs = std::fs::read_dir(input_dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("")
                    .ends_with("-raw.ts")
            })
            .collect::<Vec<_>>();
        inputs.sort();
        assert!(!inputs.is_empty());

        let page_url = std::env::var("RUST_REMUX_PAGE_URL")
            .unwrap_or_else(|_| "https://www.yangshipin.cn/tv/home?pid=600001859".to_string());
        let mut runtime = CmgRuntime::load_for_page(&page_url).unwrap();
        let media_tag_id = "1778822953341";
        runtime.prime(media_tag_id).unwrap();
        let mut video_state = CmgVideoState::default();
        let mut mux_state = TsMuxState::default();
        let mut all = Vec::new();
        let mut summaries = Vec::new();
        for input in inputs {
            let bytes = std::fs::read(&input).unwrap();
            let (output, stats) = decrypt_and_remux_ts(
                &mut runtime,
                &mut video_state,
                &mut mux_state,
                media_tag_id,
                "https://www.yangshipin.cn",
                &bytes,
            )
            .unwrap();
            let out_name = input
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap()
                .replace("-raw.ts", "-rust-remux.ts");
            std::fs::write(output_dir.join(&out_name), &output).unwrap();
            all.extend_from_slice(&output);
            summaries.push((out_name, stats));
        }
        std::fs::write(output_dir.join("all.ts"), all).unwrap();
        std::fs::write(
            output_dir.join("summary.json"),
            serde_json::to_vec_pretty(&summaries).unwrap(),
        )
        .unwrap();
    }
}
