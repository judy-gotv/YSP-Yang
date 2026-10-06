pub const KEYGEN_WASM: &[u8] = include_bytes!("../assets/keygen_bg.wasm");
pub const TICKET_WASM: &[u8] = include_bytes!("../assets/ticket.wasm");
pub const CMG_WASM: &[u8] = include_bytes!("../assets/cmg.wasm");
pub const CMG_WORKER_JS: &str = include_str!("../assets/cmg.worker.js");
pub const HLS_CMG_JS: &str = include_str!("../assets/hls.cmg.js");
pub const CMG_PLAYER_JSON: &str = include_str!("../assets/CMGPlayer.json");

pub fn embedded_cmg_worker_wasm() -> anyhow::Result<Vec<u8>> {
    let marker = "wasmBinaryFile=\"data:application/octet-stream;base64,";
    let start = CMG_WORKER_JS
        .find(marker)
        .ok_or_else(|| anyhow::anyhow!("embedded CMG worker wasm marker not found"))?
        + marker.len();
    let end = CMG_WORKER_JS[start..]
        .find('"')
        .ok_or_else(|| anyhow::anyhow!("embedded CMG worker wasm terminator not found"))?
        + start;
    Ok(base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        &CMG_WORKER_JS[start..end],
    )?)
}

pub fn embedded_cmg_worker_static_data() -> anyhow::Result<Vec<u8>> {
    let prerun = CMG_WORKER_JS
        .find("__ATPRERUN__.push(function(){")
        .ok_or_else(|| anyhow::anyhow!("CMG worker prerun block not found"))?;
    let mut cursor = prerun;
    let mut chunks = Vec::new();
    let mut total_len = 0usize;
    while let Some(relative) = CMG_WORKER_JS[cursor..].find("HEAPU8.set([") {
        let array_start = cursor + relative + "HEAPU8.set([".len();
        let array_end = CMG_WORKER_JS[array_start..]
            .find(']')
            .ok_or_else(|| anyhow::anyhow!("CMG worker static data array terminator not found"))?
            + array_start;
        let after = &CMG_WORKER_JS[array_end + 1..CMG_WORKER_JS.len().min(array_end + 64)];
        let Some(offset) = parse_eb_offset(after.trim_start()) else {
            cursor = array_end + 1;
            continue;
        };
        let mut chunk = Vec::new();
        for raw in CMG_WORKER_JS[array_start..array_end].split(',') {
            let value = raw.trim();
            if value.is_empty() {
                continue;
            }
            chunk.push(value.parse::<u8>()?);
        }
        total_len = total_len.max(offset + chunk.len());
        chunks.push((offset, chunk));
        cursor = array_end + 1;
    }
    if chunks.is_empty() {
        return Err(anyhow::anyhow!("CMG worker static data arrays not found"));
    }
    let mut output = vec![0u8; total_len];
    for (offset, chunk) in chunks {
        output[offset..offset + chunk.len()].copy_from_slice(&chunk);
    }
    for offset in embedded_cmg_worker_relocations()? {
        let end = offset + 4;
        let bytes = output
            .get_mut(offset..end)
            .ok_or_else(|| anyhow::anyhow!("CMG relocation out of range offset={offset}"))?;
        let value = u32::from_le_bytes(bytes.try_into().expect("relocation has four bytes"));
        bytes.copy_from_slice(&value.wrapping_add(EB_BASE as u32).to_le_bytes());
    }
    Ok(output)
}

const EB_BASE: usize = 6_309_392;

fn embedded_cmg_worker_relocations() -> anyhow::Result<Vec<usize>> {
    let marker = "for(var e=0;e<A.length;e++)HEAPU32[eb+A[e]>>2]=HEAPU32[eb+A[e]>>2]+eb";
    let marker_pos = CMG_WORKER_JS
        .find(marker)
        .ok_or_else(|| anyhow::anyhow!("CMG worker relocation marker not found"))?;
    let block_start = CMG_WORKER_JS[..marker_pos]
        .rfind("var A=[];")
        .ok_or_else(|| anyhow::anyhow!("CMG worker relocation array start not found"))?
        + "var A=[];".len();
    let block = &CMG_WORKER_JS[block_start..marker_pos];
    let mut cursor = 0usize;
    let mut offsets = Vec::new();
    while let Some(relative) = block[cursor..].find("concat([") {
        let array_start = cursor + relative + "concat([".len();
        let array_end = block[array_start..]
            .find(']')
            .ok_or_else(|| anyhow::anyhow!("CMG worker relocation concat terminator not found"))?
            + array_start;
        for raw in block[array_start..array_end].split(',') {
            let value = raw.trim();
            if value.is_empty() {
                continue;
            }
            offsets.push(parse_js_usize(value)?);
        }
        cursor = array_end + 1;
    }
    if offsets.is_empty() {
        let value = block.trim();
        if value.is_empty() {
            return Err(anyhow::anyhow!("CMG worker relocation array is empty"));
        }
        for raw in value.split(',') {
            let value = raw.trim();
            if value.is_empty() {
                continue;
            }
            offsets.push(parse_js_usize(value)?);
        }
    }
    if offsets.is_empty() {
        return Err(anyhow::anyhow!("CMG worker relocation array is empty"));
    }
    Ok(offsets)
}

fn parse_js_usize(value: &str) -> anyhow::Result<usize> {
    if let Some((base, exp)) = value.split_once('e') {
        let base = base.parse::<usize>()?;
        let exp = exp.parse::<u32>()?;
        return Ok(base * 10usize.pow(exp));
    }
    Ok(value.parse::<usize>()?)
}

fn parse_eb_offset(text: &str) -> Option<usize> {
    let rest = text.strip_prefix(",eb+")?;
    let end = rest
        .find(|ch: char| !ch.is_ascii_digit() && ch != 'e')
        .unwrap_or(rest.len());
    let value = &rest[..end];
    parse_js_usize(value).ok()
}
