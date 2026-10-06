use anyhow::{anyhow, Result};
use serde::Serialize;

use crate::cmg::CmgRuntime;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct TsDecryptStats {
    pub video_pid: u16,
    pub nal_count: usize,
    pub decoded_nals: usize,
    pub changed_nals: usize,
    pub changed_bytes: usize,
    pub shorter_nals: usize,
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
struct PesChunk {
    bytes: Vec<u8>,
    packet_start: usize,
    packet_offset: usize,
}

#[derive(Debug)]
struct ParsedPes {
    pes: Vec<u8>,
    payload_start: usize,
    maps: Vec<PesMap>,
}

#[derive(Debug)]
struct PesMap {
    pes_start: usize,
    pes_end: usize,
    packet_start: usize,
    packet_offset: usize,
}

#[derive(Debug)]
struct NalRange {
    prefix_start: usize,
    start: usize,
    end: usize,
}

pub fn decrypt_ts_segment(
    runtime: &mut CmgRuntime,
    media_tag_id: &str,
    active_url: &str,
    input: &[u8],
) -> Result<(Vec<u8>, TsDecryptStats)> {
    let packets = parse_ts_packets(input)?;
    let video_pid = find_h264_pid(input, &packets)?;
    let mut output = input.to_vec();
    let mut current_pes = Vec::new();
    let mut stats = TsDecryptStats {
        video_pid,
        ..TsDecryptStats::default()
    };

    for packet in packets.iter().filter(|packet| packet.pid == video_pid) {
        if reached_test_limit(&stats) {
            break;
        }
        if packet.payload_offset >= packet.payload_end {
            continue;
        }
        if packet.pusi {
            flush_pes(
                runtime,
                media_tag_id,
                active_url,
                &mut current_pes,
                &mut output,
                &mut stats,
            )?;
        }
        current_pes.push(PesChunk {
            bytes: input[packet.payload_offset..packet.payload_end].to_vec(),
            packet_start: packet.offset,
            packet_offset: packet.payload_offset,
        });
    }
    flush_pes(
        runtime,
        media_tag_id,
        active_url,
        &mut current_pes,
        &mut output,
        &mut stats,
    )?;
    fix_continuity_counters(&mut output, video_pid);

    Ok((output, stats))
}

fn flush_pes(
    runtime: &mut CmgRuntime,
    media_tag_id: &str,
    active_url: &str,
    chunks: &mut Vec<PesChunk>,
    output: &mut [u8],
    stats: &mut TsDecryptStats,
) -> Result<()> {
    if chunks.is_empty() {
        return Ok(());
    }
    let parsed = match parse_pes_payload(chunks) {
        Some(value) => value,
        None => {
            chunks.clear();
            return Ok(());
        }
    };
    chunks.clear();

    let rebuilt = rebuild_pes(runtime, media_tag_id, active_url, &parsed, stats)?;
    repacketize_pes(output, &parsed.maps, &rebuilt)?;
    Ok(())
}

fn rebuild_pes(
    runtime: &mut CmgRuntime,
    media_tag_id: &str,
    active_url: &str,
    parsed: &ParsedPes,
    stats: &mut TsDecryptStats,
) -> Result<Vec<u8>> {
    let nals = find_annex_b_nals(&parsed.pes, parsed.payload_start);
    if nals.is_empty() {
        return Ok(parsed.pes.clone());
    }

    let mut rebuilt = Vec::with_capacity(parsed.pes.len());
    rebuilt.extend_from_slice(&parsed.pes[..parsed.payload_start]);
    let mut cursor = parsed.payload_start;
    for nal in nals {
        if reached_test_limit(stats) {
            break;
        }
        if nal.prefix_start > cursor {
            rebuilt.extend_from_slice(&parsed.pes[cursor..nal.prefix_start]);
        }
        rebuilt.extend_from_slice(&parsed.pes[nal.prefix_start..nal.start]);
        let data = &parsed.pes[nal.start..nal.end];
        if data.is_empty() {
            continue;
        }
        stats.nal_count += 1;
        runtime.update(media_tag_id)?;

        let nal_type = data[0] & 0x1f;
        if !matches!(nal_type, 1 | 5) {
            rebuilt.extend_from_slice(data);
            cursor = nal.end;
            continue;
        }
        if let Some(limit) = decrypt_nal_limit() {
            if stats.decoded_nals >= limit {
                rebuilt.extend_from_slice(data);
                cursor = nal.end;
                break;
            }
        }

        stats.decoded_nals += 1;
        let decoded = runtime.module_dec_live(media_tag_id, data, active_url)?;
        let byte_diff = count_byte_diff(data, &decoded);
        if byte_diff > 0 {
            stats.changed_nals += 1;
            stats.changed_bytes += byte_diff;
        }
        if decoded.len() < data.len() {
            stats.shorter_nals += 1;
        }
        if decoded.len() > data.len() {
            return Err(anyhow!(
                "CMG output grew from {} to {} bytes for H264 NAL type {nal_type}",
                data.len(),
                decoded.len()
            ));
        }
        rebuilt.extend_from_slice(&decoded);
        cursor = nal.end;
    }
    if cursor < parsed.pes.len() {
        rebuilt.extend_from_slice(&parsed.pes[cursor..]);
    }
    update_pes_packet_length(&mut rebuilt);
    Ok(rebuilt)
}

fn repacketize_pes(output: &mut [u8], maps: &[PesMap], pes: &[u8]) -> Result<()> {
    let max_payload: usize = maps.iter().map(|_| 184usize).sum();
    if pes.len() > max_payload {
        return Err(anyhow!(
            "rebuilt PES is larger than available TS payload: {} > {}",
            pes.len(),
            max_payload
        ));
    }

    let mut cursor = 0usize;
    for map in maps {
        let packet_start = map.packet_start;
        let packet_end = packet_start + 188;
        let remaining = pes.len().saturating_sub(cursor);
        let payload_len = remaining.min(184);
        let original_header = [
            output[packet_start],
            output[packet_start + 1],
            output[packet_start + 2],
            output[packet_start + 3],
        ];
        output[packet_start..packet_end].fill(0xff);
        output[packet_start] = original_header[0];
        output[packet_start + 1] = original_header[1];
        output[packet_start + 2] = original_header[2];

        if payload_len == 0 {
            output[packet_start + 3] = (original_header[3] & 0xcf) | 0x20;
            output[packet_start + 4] = 183;
            output[packet_start + 5] = 0;
            continue;
        }

        if payload_len == 184 {
            output[packet_start + 3] = (original_header[3] & 0xcf) | 0x10;
            let payload_start = packet_start + 4;
            output[payload_start..payload_start + payload_len]
                .copy_from_slice(&pes[cursor..cursor + payload_len]);
        } else {
            output[packet_start + 3] = (original_header[3] & 0xcf) | 0x30;
            let adaptation_len = 183usize - payload_len;
            output[packet_start + 4] = adaptation_len as u8;
            if adaptation_len > 0 {
                output[packet_start + 5] = 0;
            }
            let payload_start = packet_start + 5 + adaptation_len;
            output[payload_start..payload_start + payload_len]
                .copy_from_slice(&pes[cursor..cursor + payload_len]);
        }
        cursor += payload_len;
    }
    Ok(())
}

fn update_pes_packet_length(pes: &mut [u8]) {
    if pes.len() < 6 {
        return;
    }
    let original = ((pes[4] as usize) << 8) | pes[5] as usize;
    if original == 0 {
        return;
    }
    let Some(length) = pes.len().checked_sub(6) else {
        return;
    };
    if length > u16::MAX as usize {
        pes[4] = 0;
        pes[5] = 0;
    } else {
        pes[4] = (length >> 8) as u8;
        pes[5] = length as u8;
    }
}

#[allow(dead_code)]
fn pes_offset_to_ts_offset(maps: &[PesMap], pes_offset: usize) -> Option<usize> {
    let entry = maps
        .iter()
        .find(|entry| entry.pes_start <= pes_offset && pes_offset < entry.pes_end)?;
    Some(entry.packet_offset + (pes_offset - entry.pes_start))
}

fn fix_continuity_counters(output: &mut [u8], pid: u16) {
    let mut next_counter = None;
    for offset in (0..output.len()).step_by(188) {
        if output[offset] != 0x47 {
            continue;
        }
        let packet_pid = (((output[offset + 1] & 0x1f) as u16) << 8) | output[offset + 2] as u16;
        if packet_pid != pid {
            continue;
        }
        let afc = (output[offset + 3] >> 4) & 0x03;
        let has_payload = afc == 1 || afc == 3;
        let counter = next_counter.get_or_insert(output[offset + 3] & 0x0f);
        if has_payload {
            output[offset + 3] = (output[offset + 3] & 0xf0) | *counter;
            *counter = (*counter + 1) & 0x0f;
        } else {
            let previous = counter.wrapping_sub(1) & 0x0f;
            output[offset + 3] = (output[offset + 3] & 0xf0) | previous;
        }
    }
}

fn reached_test_limit(stats: &TsDecryptStats) -> bool {
    decrypt_nal_limit()
        .map(|limit| stats.decoded_nals >= limit)
        .unwrap_or(false)
}

fn decrypt_nal_limit() -> Option<usize> {
    #[cfg(test)]
    {
        std::env::var("RUST_TS_DECRYPT_LIMIT_NALS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
    }
    #[cfg(not(test))]
    {
        None
    }
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

fn find_h264_pid(input: &[u8], packets: &[Packet]) -> Result<u16> {
    let mut pmt_pid = None;
    for packet in packets {
        if packet.pid == 0 && packet.pusi && packet.payload_offset < packet.payload_end {
            pmt_pid = parse_pat(&input[packet.payload_offset..packet.payload_end]);
            if pmt_pid.is_some() {
                break;
            }
        }
    }
    let pmt_pid = pmt_pid.ok_or_else(|| anyhow!("PAT did not expose PMT PID"))?;
    for packet in packets {
        if packet.pid == pmt_pid && packet.pusi && packet.payload_offset < packet.payload_end {
            for (pid, stream_type) in parse_pmt(&input[packet.payload_offset..packet.payload_end]) {
                if stream_type == 0x1b {
                    return Ok(pid);
                }
            }
        }
    }
    Err(anyhow!("PMT did not expose H264 PID"))
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

fn parse_pmt(payload: &[u8]) -> Vec<(u16, u8)> {
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
        streams.push((pid, stream_type));
        offset += 5 + info_length;
    }
    streams
}

fn parse_pes_payload(chunks: &[PesChunk]) -> Option<ParsedPes> {
    let total_len: usize = chunks.iter().map(|chunk| chunk.bytes.len()).sum();
    let mut pes = Vec::with_capacity(total_len);
    let mut maps = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        let pes_start = pes.len();
        pes.extend_from_slice(&chunk.bytes);
        maps.push(PesMap {
            pes_start,
            pes_end: pes.len(),
            packet_start: chunk.packet_start,
            packet_offset: chunk.packet_offset,
        });
    }
    if pes.len() < 9 || pes[0] != 0 || pes[1] != 0 || pes[2] != 1 {
        return None;
    }
    let payload_start = 9 + pes[8] as usize;
    if payload_start > pes.len() {
        return None;
    }
    Some(ParsedPes {
        pes,
        payload_start,
        maps,
    })
}

fn find_annex_b_nals(pes: &[u8], payload_start: usize) -> Vec<NalRange> {
    let mut starts = Vec::new();
    let mut index = payload_start;
    while index + 3 < pes.len() {
        if pes[index] == 0 && pes[index + 1] == 0 && pes[index + 2] == 1 {
            starts.push((index, index + 3));
            index += 3;
        } else if index + 4 < pes.len()
            && pes[index] == 0
            && pes[index + 1] == 0
            && pes[index + 2] == 0
            && pes[index + 3] == 1
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
            .unwrap_or(pes.len());
        let nal_start = starts[idx].1;
        if nal_start < next_prefix {
            nals.push(NalRange {
                prefix_start: starts[idx].0,
                start: nal_start,
                end: next_prefix,
            });
        }
    }
    nals
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
    fn decrypts_sample_ts_without_changing_segment_shape() {
        let mut runtime = CmgRuntime::load().unwrap();
        runtime.prime("1778267239522").unwrap();
        let (output, stats) = decrypt_ts_segment(
            &mut runtime,
            "1778267239522",
            "https://www.yangshipin.cn",
            SAMPLE_TS,
        )
        .unwrap();
        assert_eq!(output.len(), SAMPLE_TS.len());
        assert_eq!(stats.video_pid, 256);
        if std::env::var_os("RUST_TS_DECRYPT_LIMIT_NALS").is_none() {
            assert_eq!(stats.nal_count, 627);
            assert_eq!(stats.decoded_nals, 125);
            assert_eq!(stats.changed_nals, 125);
            assert!(stats.changed_bytes > 100_000);
        } else {
            assert!(stats.nal_count > 0);
            assert!(stats.decoded_nals > 0);
        }
        if std::env::var_os("RUST_WRITE_TS_FIXTURE").is_some() {
            let path = std::path::Path::new("../run/rust-verify/rust-direct-ts-filtered-15.ts");
            std::fs::write(path, output).unwrap();
        }
    }
}
