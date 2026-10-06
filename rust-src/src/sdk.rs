use std::collections::HashMap;

use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use serde::Deserialize;
use serde::Serialize;
use wasmtime::{Caller, Engine, Linker, Memory, Module, Store};

use crate::{
    assets::KEYGEN_WASM,
    constants::{ACTIVE_URL, USER_AGENT, YSPAPPID},
    sign::{canonical_sorted_string, md5_hex, random_string},
};

const TOKEN_ENDPOINT: &str = "https://h5access.yangshipin.cn/web/open/token";
const VAPPID: &str = "59306155";
const VSECRET: &str = "b42702bf7309a179d102f3d51b1add2fda0bc7ada64cb801";
const REQUEST_ID_PREFIX: &str = "999999";

#[derive(Debug, Clone)]
pub struct SdkState {
    pub guid: String,
    pub token: String,
    pub yspappid: String,
    pub input: String,
    pub ts: String,
    pub version: String,
    pub host: String,
    pub protocol: String,
}

impl SdkState {
    pub fn new(
        guid: impl Into<String>,
        token: impl Into<String>,
        input: impl Into<String>,
    ) -> Self {
        Self {
            guid: guid.into(),
            token: token.into(),
            yspappid: YSPAPPID.to_string(),
            input: input.into(),
            ts: now_ms_string(),
            version: "v1".to_string(),
            host: "www.yangshipin.cn".to_string(),
            protocol: "https:".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SdkHeaders {
    pub yspsdkinput: String,
    pub yspsdksign: String,
    pub seq_id: u32,
    pub request_id: String,
    pub signature_hex: String,
    pub input: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct OpenapiToken {
    pub status: u16,
    pub url: String,
    pub rnd: String,
    pub ts: String,
    pub token: String,
    pub expire: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct OpenapiTokenResponse {
    #[serde(default)]
    data: Option<OpenapiTokenData>,
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    expire: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct OpenapiTokenData {
    #[serde(default)]
    token: String,
    #[serde(default)]
    expire: Option<u64>,
}

struct WasmState {
    values: HashMap<String, String>,
    heap: Vec<Option<String>>,
}

impl WasmState {
    fn from_sdk(state: &SdkState) -> Self {
        let mut values = HashMap::new();
        values.insert("cctvh5openapi.state.guid".to_string(), state.guid.clone());
        values.insert(
            "cctvh5openapi.state.yspappid".to_string(),
            state.yspappid.clone(),
        );
        values.insert(
            "cctvh5openapi.state.version".to_string(),
            state.version.clone(),
        );
        values.insert("window.location.host".to_string(), state.host.clone());
        values.insert(
            "window.location.protocol".to_string(),
            state.protocol.clone(),
        );
        values.insert("cctvh5openapi.state.token".to_string(), state.token.clone());
        values.insert("cctvh5openapi.state.input".to_string(), state.input.clone());
        values.insert("cctvh5openapi.state.ts".to_string(), state.ts.clone());

        let mut heap = vec![None; 132];
        heap[129] = Some("null".to_string());
        heap[130] = Some("true".to_string());
        heap[131] = Some("false".to_string());
        Self { values, heap }
    }

    fn add_heap_string(&mut self, value: String) -> i32 {
        self.heap.push(Some(value));
        (self.heap.len() - 1) as i32
    }

    fn heap_string(&self, idx: i32) -> Option<String> {
        self.heap.get(idx as usize).and_then(Clone::clone)
    }

    fn drop_heap(&mut self, idx: i32) {
        if idx >= 132 {
            if let Some(slot) = self.heap.get_mut(idx as usize) {
                *slot = None;
            }
        }
    }
}

pub fn canonical_body_md5<I, K, V>(pairs: I) -> String
where
    I: IntoIterator<Item = (K, V)>,
    K: Into<String>,
    V: Into<String>,
{
    md5_hex(canonical_sorted_string(pairs))
}

pub fn build_input(body_md5: &str, guid: &str, seq_id: u32, request_id: &str) -> String {
    format!("{body_md5}-{guid}-{seq_id}-{request_id}")
}

pub fn build_request_id() -> String {
    format!(
        "{REQUEST_ID_PREFIX}{}{}",
        random_string(10),
        now_ms_string()
    )
}

pub async fn fetch_openapi_token(http: &Client, guid: &str) -> Result<OpenapiToken> {
    let mut state = SdkState::new(guid, "", "");
    state.ts = now_ms_string();
    let mut keygen = KeygenWasm::load(state.clone())?;
    let rnd = keygen.get_token_rnd()?;
    let url = reqwest::Url::parse_with_params(
        TOKEN_ENDPOINT,
        [
            ("yspappid", state.yspappid.as_str()),
            ("guid", guid),
            ("vappid", VAPPID),
            ("vsecret", VSECRET),
            ("raw", "1"),
            ("version", state.version.as_str()),
            ("ts", state.ts.as_str()),
            ("rnd", rnd.as_str()),
        ],
    )?;
    let response = http
        .get(url.clone())
        .header("accept", "application/json, text/plain, */*")
        .header("origin", ACTIVE_URL)
        .header("referer", format!("{ACTIVE_URL}/"))
        .header("user-agent", USER_AGENT)
        .send()
        .await?;
    let status = response.status();
    let text = response.text().await?;
    let parsed: OpenapiTokenResponse = serde_json::from_str(&text).with_context(|| {
        format!(
            "parse h5access token response: {}",
            text.chars().take(500).collect::<String>()
        )
    })?;
    let token = parsed
        .data
        .as_ref()
        .map(|data| data.token.clone())
        .filter(|value| !value.is_empty())
        .or(parsed.token)
        .unwrap_or_default();
    if !status.is_success() || token.is_empty() {
        anyhow::bail!(
            "h5access token failed status={}: {}",
            status.as_u16(),
            text.chars().take(500).collect::<String>()
        );
    }
    let expire = parsed.data.and_then(|data| data.expire).or(parsed.expire);
    Ok(OpenapiToken {
        status: status.as_u16(),
        url: url.to_string(),
        rnd,
        ts: state.ts,
        token,
        expire,
    })
}

pub fn sign_with_token(
    state: SdkState,
    seq_id: u32,
    request_id: impl Into<String>,
) -> Result<SdkHeaders> {
    let request_id = request_id.into();
    let input = state.input.clone();
    let signature_hex = KeygenWasm::load(state)?.get_signature_hex()?;
    let yspsdkinput = input.split('-').next().unwrap_or("").to_string();
    Ok(SdkHeaders {
        yspsdkinput,
        yspsdksign: format!("{signature_hex}-{input}"),
        seq_id,
        request_id,
        signature_hex,
        input,
    })
}

struct KeygenWasm {
    store: Store<WasmState>,
    memory: Memory,
    get_signature: wasmtime::TypedFunc<i32, ()>,
    get_token_rnd: wasmtime::TypedFunc<i32, ()>,
    add_stack_pointer: wasmtime::TypedFunc<i32, i32>,
    free: wasmtime::TypedFunc<(i32, i32, i32), ()>,
}

impl KeygenWasm {
    fn load(state: SdkState) -> Result<Self> {
        let engine = Engine::default();
        let module = Module::from_binary(&engine, KEYGEN_WASM)?;
        let mut linker = Linker::new(&engine);

        linker.func_wrap(
            "wbg",
            "__wbg_get_9c1840f7ecd81363",
            |mut caller: Caller<'_, WasmState>, ptr: i32, len: i32| -> Result<i32> {
                let key = read_string(&mut caller, ptr, len)?;
                let value = caller.data().values.get(&key).cloned().unwrap_or_default();
                Ok(caller.data_mut().add_heap_string(value))
            },
        )?;
        linker.func_wrap(
            "wbg",
            "__wbindgen_string_get",
            |mut caller: Caller<'_, WasmState>, out_ptr: i32, obj_idx: i32| -> Result<()> {
                let value = caller.data().heap_string(obj_idx);
                let (ptr, len) = if let Some(value) = value {
                    let bytes = value.as_bytes();
                    let malloc = caller
                        .get_export("__wbindgen_malloc")
                        .and_then(|export| export.into_func())
                        .ok_or_else(|| anyhow!("missing __wbindgen_malloc export"))?
                        .typed::<(i32, i32), i32>(&caller)?;
                    let ptr = malloc.call(&mut caller, (bytes.len() as i32, 1))?;
                    let memory = memory(&mut caller)?;
                    let data = memory.data_mut(&mut caller);
                    let start = ptr as usize;
                    let end = start + bytes.len();
                    data.get_mut(start..end)
                        .ok_or_else(|| anyhow!("wasm string allocation out of range"))?
                        .copy_from_slice(bytes);
                    (ptr, bytes.len() as i32)
                } else {
                    (0, 0)
                };
                let memory = memory(&mut caller)?;
                let data = memory.data_mut(&mut caller);
                write_i32_le(data, out_ptr, ptr)?;
                write_i32_le(data, out_ptr + 4, len)?;
                Ok(())
            },
        )?;
        linker.func_wrap(
            "wbg",
            "__wbindgen_object_drop_ref",
            |mut caller: Caller<'_, WasmState>, idx: i32| {
                caller.data_mut().drop_heap(idx);
            },
        )?;

        let mut store = Store::new(&engine, WasmState::from_sdk(&state));
        let instance = linker.instantiate(&mut store, &module)?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| anyhow!("missing keygen memory export"))?;
        let get_signature = instance.get_typed_func::<i32, ()>(&mut store, "get_signature")?;
        let get_token_rnd = instance.get_typed_func::<i32, ()>(&mut store, "get_token_rnd")?;
        let add_stack_pointer =
            instance.get_typed_func::<i32, i32>(&mut store, "__wbindgen_add_to_stack_pointer")?;
        let free = instance.get_typed_func::<(i32, i32, i32), ()>(&mut store, "__wbindgen_free")?;
        Ok(Self {
            store,
            memory,
            get_signature,
            get_token_rnd,
            add_stack_pointer,
            free,
        })
    }

    #[allow(dead_code)]
    fn get_token_rnd(&mut self) -> Result<String> {
        self.call_string_export(self.get_token_rnd.clone())
    }

    fn get_signature_hex(&mut self) -> Result<String> {
        self.call_string_export(self.get_signature.clone())
    }

    fn call_string_export(&mut self, func: wasmtime::TypedFunc<i32, ()>) -> Result<String> {
        let ret_ptr = self.add_stack_pointer.call(&mut self.store, -16)?;
        let mut ptr = 0;
        let mut len = 0;
        let result = (|| -> Result<String> {
            func.call(&mut self.store, ret_ptr)?;
            let data = self.memory.data(&self.store);
            ptr = read_i32_le(data, ret_ptr)?;
            len = read_i32_le(data, ret_ptr + 4)?;
            let start = ptr as usize;
            let end = start + len as usize;
            let bytes = data
                .get(start..end)
                .ok_or_else(|| anyhow!("wasm return string out of range"))?;
            String::from_utf8(bytes.to_vec()).context("wasm returned non-utf8 string")
        })();
        let _ = self.add_stack_pointer.call(&mut self.store, 16);
        if ptr > 0 && len >= 0 {
            let _ = self.free.call(&mut self.store, (ptr, len, 1));
        }
        result
    }
}

fn memory(caller: &mut Caller<'_, WasmState>) -> Result<Memory> {
    caller
        .get_export("memory")
        .and_then(|export| export.into_memory())
        .ok_or_else(|| anyhow!("missing wasm memory export"))
}

fn read_string(caller: &mut Caller<'_, WasmState>, ptr: i32, len: i32) -> Result<String> {
    let memory = memory(caller)?;
    let data = memory.data(caller);
    let start = ptr as usize;
    let end = start + len as usize;
    let bytes = data
        .get(start..end)
        .ok_or_else(|| anyhow!("wasm read string out of range"))?;
    String::from_utf8(bytes.to_vec()).context("wasm input string is non-utf8")
}

fn write_i32_le(data: &mut [u8], ptr: i32, value: i32) -> Result<()> {
    let start = ptr as usize;
    let end = start + 4;
    data.get_mut(start..end)
        .ok_or_else(|| anyhow!("wasm i32 write out of range"))?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn read_i32_le(data: &[u8], ptr: i32) -> Result<i32> {
    let start = ptr as usize;
    let end = start + 4;
    let bytes = data
        .get(start..end)
        .ok_or_else(|| anyhow!("wasm i32 read out of range"))?;
    Ok(i32::from_le_bytes(bytes.try_into().expect("slice length")))
}

fn now_ms_string() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keygen_signature_matches_node_fixture() {
        let guid = "moxfxpzd_dto2apb3j9j";
        let body_md5 = "a9f1e4f4aa672cbed61eec91fcade54e";
        let request_id = "999999UWCr4euHl71778337551617";
        let input = build_input(body_md5, guid, 1, request_id);
        let state = SdkState::new(guid, "372067b35cb4378ef8c86aab94a23a52", input);
        let headers = sign_with_token(state, 1, request_id).unwrap();
        assert_eq!(headers.yspsdkinput, body_md5);
        assert_eq!(headers.signature_hex, "7a13aefe8715b8211f729059f36bc57c");
        assert_eq!(
            headers.yspsdksign,
            "7a13aefe8715b8211f729059f36bc57c-a9f1e4f4aa672cbed61eec91fcade54e-moxfxpzd_dto2apb3j9j-1-999999UWCr4euHl71778337551617"
        );
    }
}
