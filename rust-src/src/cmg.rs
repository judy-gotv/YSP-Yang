use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use wasmtime::{Caller, Engine, Linker, Memory, Ref, Store, Table};

use crate::assets::{embedded_cmg_worker_static_data, embedded_cmg_worker_wasm, CMG_PLAYER_JSON};

const INITIAL_PAGES: u32 = 256;
const MAX_PAGES: u32 = 1536;
const TABLE_MIN: u32 = 560;
const DYNAMIC_BASE: i32 = 5_260_816;
const EMT_STACK_SIZE: i32 = 1_048_576;
const EB_SIZE: i32 = 378_592;
const DYNAMIC_TOP_AFTER_RUNTIME_ALLOCS: i32 = DYNAMIC_BASE + EMT_STACK_SIZE + EB_SIZE;
const DYNAMICTOP_PTR: i32 = 17_904;
const TEMP_DOUBLE_PTR: i32 = 17_920;
const EMT_STACK_TOP: i32 = DYNAMIC_BASE;
const EB: i32 = DYNAMIC_BASE + EMT_STACK_SIZE;
const MEMORY_EXTEND: usize = 2048;
static HEAP_DUMP_INDEX: AtomicUsize = AtomicUsize::new(0);

struct CmgState {
    memory: Option<Memory>,
    table: Option<Table>,
    malloc: Option<wasmtime::TypedFunc<i32, i32>>,
    free: Option<wasmtime::TypedFunc<i32, ()>>,
    pending_fetches: Vec<PendingFetch>,
    next_fetch_handle: i32,
    temp_ret0: i32,
    function_pointers: Vec<Option<u32>>,
    frozen_now_ms: Option<f64>,
    emval: EmvalState,
    location: CmgLocation,
}

#[derive(Debug)]
struct PendingFetch {
    ptr: i32,
    url: String,
    onsuccess: i32,
    onerror: i32,
    flags: u32,
}

#[derive(Clone, Debug)]
enum EmvalValue {
    Undefined,
    Null,
    Bool(bool),
    Location,
    String(String),
    Destructors(Vec<i32>),
}

#[derive(Debug)]
struct EmvalState {
    handles: Vec<Option<EmvalValue>>,
    free: Vec<i32>,
    std_string_type: Option<i32>,
    location_handle: Option<i32>,
}

impl Default for EmvalState {
    fn default() -> Self {
        Self {
            handles: vec![
                None,
                Some(EmvalValue::Undefined),
                Some(EmvalValue::Null),
                Some(EmvalValue::Bool(true)),
                Some(EmvalValue::Bool(false)),
            ],
            free: Vec::new(),
            std_string_type: None,
            location_handle: None,
        }
    }
}

#[derive(Clone, Debug)]
struct CmgLocation {
    href: String,
    host: String,
    hostname: String,
    origin: String,
    protocol: String,
}

impl CmgLocation {
    fn from_href(href: &str) -> Self {
        let parsed = url::Url::parse(href).ok();
        let protocol = parsed
            .as_ref()
            .map(|url| format!("{}:", url.scheme()))
            .unwrap_or_else(|| "https:".to_string());
        let hostname = parsed
            .as_ref()
            .and_then(|url| url.host_str())
            .unwrap_or("www.yangshipin.cn")
            .to_string();
        let host = parsed
            .as_ref()
            .and_then(|url| {
                url.host_str().map(|host| match url.port() {
                    Some(port) => format!("{host}:{port}"),
                    None => host.to_string(),
                })
            })
            .unwrap_or_else(|| hostname.clone());
        let origin = parsed
            .as_ref()
            .map(|url| {
                let mut value = format!("{}://{}", url.scheme(), hostname);
                if let Some(port) = url.port() {
                    value.push_str(&format!(":{port}"));
                }
                value
            })
            .unwrap_or_else(|| "https://www.yangshipin.cn".to_string());
        Self {
            href: href.to_string(),
            host,
            hostname,
            origin,
            protocol,
        }
    }
}

impl Default for CmgLocation {
    fn default() -> Self {
        Self::from_href("https://www.yangshipin.cn/tv/home?pid=600099502")
    }
}

pub struct CmgRuntime {
    store: Store<CmgState>,
    memory: Memory,
    init_player: wasmtime::TypedFunc<i32, i32>,
    update_player: wasmtime::TypedFunc<i32, i32>,
    dec_live: Vec<wasmtime::TypedFunc<(i32, i32, i32, i32), i32>>,
    js_malloc: wasmtime::TypedFunc<i32, i32>,
    js_free: wasmtime::TypedFunc<i32, ()>,
    dyn_call_vi: wasmtime::TypedFunc<(i32, i32), ()>,
    vmp_tag: String,
    update_calls: usize,
    live_export_calls: usize,
    module_dec_calls: usize,
}

impl CmgRuntime {
    pub fn load() -> Result<Self> {
        Self::load_for_page("https://www.yangshipin.cn/tv/home?pid=600099502")
    }

    pub fn load_for_page(page_url: &str) -> Result<Self> {
        let engine = Engine::default();
        let wasm = embedded_cmg_worker_wasm()?;
        let module = wasmtime::Module::from_binary(&engine, &wasm)?;
        let mut store = Store::new(
            &engine,
            CmgState {
                memory: None,
                table: None,
                malloc: None,
                free: None,
                pending_fetches: Vec::new(),
                next_fetch_handle: 1,
                temp_ret0: 0,
                function_pointers: vec![None; 14],
                frozen_now_ms: frozen_now_ms(),
                emval: EmvalState::default(),
                location: CmgLocation::from_href(page_url),
            },
        );
        let memory = Memory::new(
            &mut store,
            wasmtime::MemoryType::new(INITIAL_PAGES, Some(MAX_PAGES)),
        )?;
        let table = Table::new(
            &mut store,
            wasmtime::TableType::new(wasmtime::RefType::FUNCREF, TABLE_MIN, None),
            Ref::Func(None),
        )?;
        store.data_mut().memory = Some(memory);
        store.data_mut().table = Some(table);
        {
            let data = memory.data_mut(&mut store);
            write_i32_le_raw(data, DYNAMICTOP_PTR, DYNAMIC_TOP_AFTER_RUNTIME_ALLOCS)?;
        }

        let mut linker = Linker::new(&engine);
        linker.define(&mut store, "env", "memory", memory)?;
        linker.define(&mut store, "env", "table", table)?;
        let i32_global =
            wasmtime::GlobalType::new(wasmtime::ValType::I32, wasmtime::Mutability::Const);
        let table_base = wasmtime::Global::new(&mut store, i32_global.clone(), 0i32.into())?;
        let temp_double_ptr =
            wasmtime::Global::new(&mut store, i32_global.clone(), TEMP_DOUBLE_PTR.into())?;
        let dynamic_top_ptr =
            wasmtime::Global::new(&mut store, i32_global.clone(), DYNAMICTOP_PTR.into())?;
        let emt_stack_top =
            wasmtime::Global::new(&mut store, i32_global.clone(), EMT_STACK_TOP.into())?;
        let eb = wasmtime::Global::new(&mut store, i32_global, EB.into())?;
        linker.define(&mut store, "env", "__table_base", table_base)?;
        linker.define(&mut store, "env", "a", temp_double_ptr)?;
        linker.define(&mut store, "env", "b", dynamic_top_ptr)?;
        linker.define(&mut store, "env", "c", emt_stack_top)?;
        linker.define(&mut store, "env", "d", eb)?;
        define_imports(&mut linker)?;

        let instance = linker.instantiate(&mut store, &module)?;
        {
            let data = memory.data_mut(&mut store);
            write_i32_le_raw(data, DYNAMICTOP_PTR, DYNAMIC_TOP_AFTER_RUNTIME_ALLOCS)?;
            let static_data = embedded_cmg_worker_static_data()?;
            let start = EB as usize;
            let end = start + static_data.len();
            data.get_mut(start..end)
                .ok_or_else(|| anyhow!("CMG static data out of range len={}", static_data.len()))?
                .copy_from_slice(&static_data);
        }
        let init_player = instance.get_typed_func::<i32, i32>(&mut store, "ba")?;
        let update_player = instance.get_typed_func::<i32, i32>(&mut store, "da")?;
        let dec_live = ["ea", "fa", "ga", "ha", "ia", "ja", "ka", "la", "ma"]
            .into_iter()
            .map(|name| instance.get_typed_func::<(i32, i32, i32, i32), i32>(&mut store, name))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let js_malloc = instance.get_typed_func::<i32, i32>(&mut store, "Ca")?;
        let js_free = instance.get_typed_func::<i32, ()>(&mut store, "Ba")?;
        let dyn_call_vi = instance.get_typed_func::<(i32, i32), ()>(&mut store, "Ha")?;
        let malloc = instance.get_typed_func::<i32, i32>(&mut store, "Ea")?;
        store.data_mut().malloc = Some(malloc);
        let free = instance.get_typed_func::<i32, ()>(&mut store, "Aa")?;
        store.data_mut().free = Some(free);
        trace_stage("before globalCtors");
        let global_ctors = instance.get_typed_func::<(), ()>(&mut store, "La")?;
        global_ctors.call(&mut store, ())?;
        trace_stage("after globalCtors");
        if std::env::var_os("CMG_SKIP_MAIN").is_none() {
            call_emscripten_main(&mut store, memory, &instance)?;
        }
        trace_stage("after main");
        Ok(Self {
            store,
            memory,
            init_player,
            update_player,
            dec_live,
            js_malloc,
            js_free,
            dyn_call_vi,
            vmp_tag: String::new(),
            update_calls: 0,
            live_export_calls: 0,
            module_dec_calls: 0,
        })
    }

    pub fn prime(&mut self, media_tag_id: &str) -> Result<i32> {
        trace_stage("before prime");
        let ptr = self.alloc_zeroed(media_tag_id.len() + MEMORY_EXTEND)?;
        self.write_bytes(ptr, media_tag_id.as_bytes())?;
        let ret = self.init_player.call(&mut self.store, ptr)?;
        self.drain_pending_fetches()?;
        if std::env::var_os("CMG_TRACE_STAGE").is_some() {
            eprintln!("cmg-stage prime ret={ret}");
        }
        self.free_traced(ptr);
        trace_stage("after prime");
        Ok(ret)
    }

    pub fn update(&mut self, media_tag_id: &str) -> Result<i32> {
        self.update_calls += 1;
        trace_stage("before update");
        let ptr = self.alloc_zeroed(media_tag_id.len() + MEMORY_EXTEND)?;
        self.write_bytes(ptr, media_tag_id.as_bytes())?;
        let ret = self.update_player.call(&mut self.store, ptr)?;
        if std::env::var_os("CMG_TRACE_STAGE").is_some() {
            eprintln!("cmg-stage update ret={ret} tag={:08x}", ret as u32);
        }
        if ret != 0 {
            self.vmp_tag = format!("{:08x}", ret as u32);
        }
        self.free_traced(ptr);
        trace_stage("after update");
        Ok(ret)
    }

    pub fn module_dec_live(
        &mut self,
        media_tag_id: &str,
        input: &[u8],
        active_url: &str,
    ) -> Result<Vec<u8>> {
        self.module_dec_calls += 1;
        let module_dec_call = self.module_dec_calls;
        let trace_target =
            trace_live_input_len_matches(input.len()) || trace_live_call_matches(module_dec_call);
        trace_stage("before module_dec_live");
        let data_len = input.len() + MEMORY_EXTEND;
        let data_ptr = self.alloc(data_len)?;
        self.write_zeroed(data_ptr, data_len)?;
        self.write_bytes(data_ptr, input)?;
        let active_url_len = if active_url.is_empty() {
            0
        } else {
            self.write_bytes(data_ptr + input.len() as i32, active_url.as_bytes())?;
            active_url.len() as i32
        };
        let key_len = media_tag_id.len();
        let key_ptr = self.alloc(key_len)?;
        self.write_bytes(key_ptr, media_tag_id.as_bytes())?;
        self.maybe_dump_heap_prefix("before-live")?;
        if std::env::var_os("CMG_TRACE_STAGE").is_some() {
            eprintln!(
                "cmg-stage module_dec_live malloc key={key_ptr} key_len={key_len} data={data_ptr} input_len={} active_url_len={} data_len={data_len}",
                input.len(),
                active_url_len
            );
        }
        if trace_target {
            let key_head = self.read_bytes(key_ptr, 64)?;
            eprintln!(
                "cmg-trace module_dec#{module_dec_call} before update_calls={} live_calls={} tag={} key_ptr={key_ptr} key_len={key_len} data_ptr={data_ptr} input_len={} active_url_len={} key_head={} input_head={}",
                self.update_calls,
                self.live_export_calls,
                self.vmp_tag,
                input.len(),
                active_url_len,
                hex_head(&key_head, 64),
                hex_head(input, 64)
            );
        }

        let call_result = (|| -> Result<i32> {
            if !cmg_live8_only() {
                let vmp_tag = self.vmp_tag.clone();
                for (position, ch) in vmp_tag.chars().enumerate().take(8) {
                    if matches!(ch, '0'..='6') {
                        let live_index = 7usize.saturating_sub(position);
                        let _ = self.call_live_export(
                            live_index,
                            key_ptr,
                            data_ptr,
                            input.len() as i32,
                            active_url_len,
                        )?;
                    }
                }
            }
            self.call_live_export(8, key_ptr, data_ptr, input.len() as i32, active_url_len)
        })();
        let output = match call_result {
            Ok(out_len) if out_len >= 0 => self.read_bytes(data_ptr, out_len as usize)?,
            Ok(out_len) => {
                self.free_traced(data_ptr);
                self.free_traced(key_ptr);
                return Err(anyhow!(
                    "CMG moduleDecData live returned negative length: {out_len}"
                ));
            }
            Err(error) => {
                self.free_traced(data_ptr);
                self.free_traced(key_ptr);
                return Err(error);
            }
        };
        if trace_target {
            eprintln!(
                "cmg-trace module_dec#{module_dec_call} after update_calls={} live_calls={} tag={} output_len={} output_head={}",
                self.update_calls,
                self.live_export_calls,
                self.vmp_tag,
                output.len(),
                hex_head(&output, 64)
            );
        }
        self.maybe_dump_heap_prefix("after-live")?;
        self.free_traced(data_ptr);
        self.free_traced(key_ptr);
        trace_stage("after module_dec_live");
        Ok(output)
    }

    pub fn vmp_tag(&self) -> &str {
        &self.vmp_tag
    }

    fn call_live_export(
        &mut self,
        live_index: usize,
        key_ptr: i32,
        data_ptr: i32,
        input_len: i32,
        active_url_len: i32,
    ) -> Result<i32> {
        self.live_export_calls += 1;
        let live_call = self.live_export_calls;
        let func = self
            .dec_live
            .get(live_index)
            .cloned()
            .ok_or_else(|| anyhow!("CMG live export index out of range: {live_index}"))?;
        if std::env::var_os("CMG_TRACE_STAGE").is_some() {
            eprintln!(
                "cmg-stage live{live_index} ptrs key={key_ptr} data={data_ptr} input_len={input_len} active_url_len={active_url_len} tag={}",
                self.vmp_tag
            );
        }
        let trace_target = trace_live_input_len_matches(input_len.max(0) as usize)
            || trace_live_call_matches(live_call);
        let before = if trace_target {
            Some(self.read_bytes(data_ptr, (input_len.max(0) as usize).min(64))?)
        } else {
            None
        };
        let key_before = if trace_target {
            Some(self.read_bytes(key_ptr, 64)?)
        } else {
            None
        };
        let ret = func.call(
            &mut self.store,
            (key_ptr, data_ptr, input_len, active_url_len),
        )?;
        if trace_target {
            let after_len = ret.max(0) as usize;
            let after = self.read_bytes(data_ptr, after_len.min(64))?;
            let key_after = self.read_bytes(key_ptr, 64)?;
            eprintln!(
                "cmg-trace live#{live_call} slot={live_index} ret={ret} update_calls={} tag={} key_ptr={key_ptr} data_ptr={data_ptr} input_len={input_len} active_url_len={active_url_len} key_before={} key_after={} before={} after={}",
                self.update_calls,
                self.vmp_tag,
                hex_head(key_before.as_deref().unwrap_or(&[]), 64),
                hex_head(&key_after, 64),
                hex_head(before.as_deref().unwrap_or(&[]), 64),
                hex_head(&after, 64)
            );
        }
        Ok(ret)
    }

    fn alloc_zeroed(&mut self, len: usize) -> Result<i32> {
        let ptr = self.alloc(len)?;
        let data = self.memory.data_mut(&mut self.store);
        let start = ptr as usize;
        let end = start + len;
        data.get_mut(start..end)
            .ok_or_else(|| anyhow!("CMG allocation out of range ptr={ptr} len={len}"))?
            .fill(0);
        Ok(ptr)
    }

    fn alloc(&mut self, len: usize) -> Result<i32> {
        let ptr = self.js_malloc.call(&mut self.store, len as i32)?;
        if std::env::var_os("CMG_TRACE_STAGE").is_some() {
            let dyn_top = self.dynamic_top().unwrap_or_default();
            eprintln!("cmg-stage jsmalloc len={len} ptr={ptr} dyn={dyn_top:#x}");
        }
        if ptr <= 0 {
            return Err(anyhow!("CMG jsmalloc failed for {len} bytes"));
        }
        let data_len = self.memory.data_size(&self.store);
        let start = ptr as usize;
        let end = start + len;
        if end > data_len {
            return Err(anyhow!(
                "CMG allocation out of range ptr={ptr} len={len} memory={data_len}"
            ));
        }
        Ok(ptr)
    }

    fn free_traced(&mut self, ptr: i32) {
        if std::env::var_os("CMG_TRACE_STAGE").is_some() {
            let before = self.dynamic_top().unwrap_or_default();
            let _ = self.js_free.call(&mut self.store, ptr);
            let after = self.dynamic_top().unwrap_or_default();
            eprintln!("cmg-stage jsfree ptr={ptr} dyn={before:#x}->{after:#x}");
        } else {
            let _ = self.js_free.call(&mut self.store, ptr);
        }
    }

    fn drain_pending_fetches(&mut self) -> Result<()> {
        loop {
            let Some(fetch) = self.store.data_mut().pending_fetches.pop() else {
                break;
            };
            if std::env::var_os("CMG_TRACE_STAGE").is_some() {
                eprintln!(
                    "cmg-stage fetch drain ptr={} url={} flags={:#x} onsuccess={} onerror={}",
                    fetch.ptr, fetch.url, fetch.flags, fetch.onsuccess, fetch.onerror
                );
            }
            self.complete_fetch(fetch)?;
        }
        Ok(())
    }

    fn complete_fetch(&mut self, fetch: PendingFetch) -> Result<()> {
        if fetch.url.contains("/Library/CMGPlayer.json") {
            let payload = CMG_PLAYER_JSON.as_bytes();
            let data_ptr = self.js_malloc.call(&mut self.store, payload.len() as i32)?;
            self.write_bytes(data_ptr, payload)?;
            {
                let data = self.memory.data_mut(&mut self.store);
                write_i32_le_raw(data, fetch.ptr + 12, data_ptr)?;
                write_u64_le_raw(data, fetch.ptr + 16, payload.len() as u64)?;
                write_u64_le_raw(data, fetch.ptr + 24, 0)?;
                write_u64_le_raw(data, fetch.ptr + 32, payload.len() as u64)?;
                write_u16_le_raw(data, fetch.ptr + 40, 4)?;
                write_u16_le_raw(data, fetch.ptr + 42, 200)?;
                write_c_string_raw(data, fetch.ptr + 44, 64, "OK")?;
            }
            if fetch.onsuccess != 0 {
                self.dyn_call_vi
                    .call(&mut self.store, (fetch.onsuccess, fetch.ptr))?;
            }
            return Ok(());
        }

        {
            let data = self.memory.data_mut(&mut self.store);
            write_i32_le_raw(data, fetch.ptr + 12, 0)?;
            write_u64_le_raw(data, fetch.ptr + 16, 0)?;
            write_u64_le_raw(data, fetch.ptr + 24, 0)?;
            write_u64_le_raw(data, fetch.ptr + 32, 0)?;
            write_u16_le_raw(data, fetch.ptr + 40, 4)?;
            write_u16_le_raw(data, fetch.ptr + 42, 404)?;
            write_c_string_raw(data, fetch.ptr + 44, 64, "Not Found")?;
        }
        if fetch.onerror != 0 {
            self.dyn_call_vi
                .call(&mut self.store, (fetch.onerror, fetch.ptr))?;
        }
        Ok(())
    }

    fn dynamic_top(&self) -> Result<u32> {
        let data = self.memory.data(&self.store);
        read_u32_le_raw(data, DYNAMICTOP_PTR)
    }

    fn write_bytes(&mut self, ptr: i32, bytes: &[u8]) -> Result<()> {
        let data = self.memory.data_mut(&mut self.store);
        let start = ptr as usize;
        let end = start + bytes.len();
        data.get_mut(start..end)
            .ok_or_else(|| anyhow!("CMG write out of range ptr={ptr} len={}", bytes.len()))?
            .copy_from_slice(bytes);
        Ok(())
    }

    fn write_zeroed(&mut self, ptr: i32, len: usize) -> Result<()> {
        let data = self.memory.data_mut(&mut self.store);
        let start = ptr as usize;
        let end = start + len;
        data.get_mut(start..end)
            .ok_or_else(|| anyhow!("CMG zero write out of range ptr={ptr} len={len}"))?
            .fill(0);
        Ok(())
    }

    fn read_bytes(&self, ptr: i32, len: usize) -> Result<Vec<u8>> {
        let data = self.memory.data(&self.store);
        let start = ptr as usize;
        let end = start + len;
        Ok(data
            .get(start..end)
            .ok_or_else(|| anyhow!("CMG read out of range ptr={ptr} len={len}"))?
            .to_vec())
    }

    fn maybe_dump_heap_prefix(&self, label: &str) -> Result<()> {
        let Some(dir) = std::env::var_os("CMG_DUMP_HEAP_PREFIX_DIR") else {
            return Ok(());
        };
        let index = HEAP_DUMP_INDEX.fetch_add(1, Ordering::SeqCst);
        let data = self.memory.data(&self.store);
        let len = data.len().min(7_200_000);
        let dir_path = std::path::Path::new(&dir);
        std::fs::create_dir_all(dir_path)?;
        let path = dir_path.join(format!("{index:03}-{label}.bin"));
        std::fs::write(path, &data[..len])?;
        Ok(())
    }
}

fn call_emscripten_main(
    store: &mut Store<CmgState>,
    memory: Memory,
    instance: &wasmtime::Instance,
) -> Result<()> {
    trace_stage("before main");
    let stack_alloc = instance.get_typed_func::<i32, i32>(&mut *store, "Ma")?;
    let main_func = instance.get_typed_func::<(i32, i32), i32>(&mut *store, "Da")?;
    let program = stack_c_string(store, memory, &stack_alloc, "./this.program")?;
    let argv = stack_alloc.call(&mut *store, 8)?;
    {
        let data = memory.data_mut(&mut *store);
        write_i32_le_raw(data, argv, program)?;
        write_i32_le_raw(data, argv + 4, 0)?;
    }
    let ret = main_func.call(store, (1, argv))?;
    if std::env::var_os("CMG_TRACE_STAGE").is_some() {
        eprintln!("cmg-stage main ret={ret}");
    }
    Ok(())
}

fn stack_c_string(
    store: &mut Store<CmgState>,
    memory: Memory,
    stack_alloc: &wasmtime::TypedFunc<i32, i32>,
    value: &str,
) -> Result<i32> {
    let ptr = stack_alloc.call(&mut *store, (value.len() + 1) as i32)?;
    let data = memory.data_mut(store);
    let start = ptr as usize;
    data.get_mut(start..start + value.len())
        .ok_or_else(|| anyhow!("CMG stack string write out of range"))?
        .copy_from_slice(value.as_bytes());
    data[start + value.len()] = 0;
    Ok(ptr)
}

fn define_imports(linker: &mut Linker<CmgState>) -> Result<()> {
    linker.func_wrap("env", "e", |caller: Caller<'_, CmgState>| -> i32 {
        trace_import("e", &[]);
        caller.data().temp_ret0
    })?;
    linker.func_wrap(
        "env",
        "f",
        |mut caller: Caller<'_, CmgState>, value: i32| {
            trace_import("f", &[value]);
            caller.data_mut().temp_ret0 = value;
        },
    )?;
    linker.func_wrap(
        "env",
        "g",
        |mut caller: Caller<'_, CmgState>, ptr: i32, _tz: i32| -> i32 {
            trace_import("g", &[ptr, _tz]);
            let now_ms = cmg_now_ms(caller.data()).max(0.0) as u64;
            let _ = write_i32(&mut caller, ptr, (now_ms / 1000) as i32);
            let _ = write_i32(&mut caller, ptr + 4, ((now_ms % 1000) * 1000) as i32);
            0
        },
    )?;
    linker.func_wrap(
        "env",
        "h",
        |_db: i32, _fetch: i32, _data: i32, _onsuccess: i32, _onerror: i32| {},
    )?;
    linker.func_wrap("env", "i", |caller: Caller<'_, CmgState>| -> f64 {
        trace_import("i", &[]);
        cmg_now_ms(caller.data())
    })?;
    linker.func_wrap("env", "j", || {})?;
    linker.func_wrap(
        "env",
        "k",
        |mut caller: Caller<'_, CmgState>, func: i32, a: i32| {
            let _ = call_table_v(&mut caller, func, &[a]);
        },
    )?;
    linker.func_wrap("env", "l", |_func: i32| {})?;
    linker.func_wrap(
        "env",
        "m",
        |_func: i32, _a: i32, _b: i32, _c: i32, _d: i32| -> i32 { 0 },
    )?;
    linker.func_wrap("env", "n", |_func: i32, _a: i32, _b: i32, _c: i32| -> i32 {
        0
    })?;
    linker.func_wrap("env", "o", |_requested_size: i32| -> i32 { 0 })?;
    linker.func_wrap("env", "p", |_func: i32, _a: i32, _b: i32| -> i32 { 0 })?;
    linker.func_wrap("env", "q", || {})?;
    linker.func_wrap(
        "env",
        "r",
        |mut caller: Caller<'_, CmgState>, fetch: i32| {
            trace_import("r", &[fetch]);
            let _ = emscripten_start_fetch(&mut caller, fetch);
        },
    )?;
    linker.func_wrap(
        "env",
        "s",
        |mut caller: Caller<'_, CmgState>, requested_size: i32| -> i32 {
            resize_heap(&mut caller, requested_size).unwrap_or(0)
        },
    )?;
    linker.func_wrap(
        "env",
        "t",
        |mut caller: Caller<'_, CmgState>, dst: i32, src: i32, len: i32| -> i32 {
            let _ = copy_within_memory(&mut caller, dst, src, len);
            dst
        },
    )?;
    linker.func_wrap(
        "env",
        "u",
        |mut caller: Caller<'_, CmgState>, flags: i32, varargs: i32| {
            trace_import("u", &[flags, varargs]);
            log_from_wasm(&mut caller, flags, varargs);
        },
    )?;
    linker.func_wrap("env", "v", || -> i32 { 1 })?;
    linker.func_wrap("env", "w", |caller: Caller<'_, CmgState>| -> i32 {
        trace_import("w", &[]);
        caller
            .data()
            .memory
            .map(|memory| memory.data_size(&caller) as i32)
            .unwrap_or(0)
    })?;
    linker.func_wrap(
        "env",
        "x",
        |_flags: i32, _out: i32, _maxbytes: i32| -> i32 { 0 },
    )?;
    linker.func_wrap(
        "env",
        "y",
        |_func: i32, _a: i32, _b: f64, _c: i32, _d: i32, _e: i32, _f: i32| -> i32 { 0 },
    )?;
    linker.func_wrap(
        "env",
        "z",
        |mut caller: Caller<'_, CmgState>, index: i32, arg: i32| -> i32 {
            trace_import("z", &[index, arg]);
            asm_const_ii(&mut caller, index, arg).unwrap_or(0)
        },
    )?;
    linker.func_wrap("env", "A", || {})?;
    linker.func_wrap(
        "env",
        "B",
        |mut caller: Caller<'_, CmgState>, type_id: i32, ptr: i32| -> i32 {
            trace_import("B", &[type_id, ptr]);
            emval_take_value(&mut caller, type_id, ptr).unwrap_or(0)
        },
    )?;
    linker.func_wrap(
        "env",
        "C",
        |mut caller: Caller<'_, CmgState>, destructors: i32| {
            trace_import("C", &[destructors]);
            emval_run_destructors(&mut caller, destructors);
        },
    )?;
    linker.func_wrap(
        "env",
        "D",
        |mut caller: Caller<'_, CmgState>, obj: i32, prop: i32| -> i32 {
            trace_import("D", &[obj, prop]);
            emval_get_property(&mut caller, obj, prop).unwrap_or(0)
        },
    )?;
    linker.func_wrap(
        "env",
        "E",
        |mut caller: Caller<'_, CmgState>, name: i32| -> i32 {
            trace_import("E", &[name]);
            emval_get_global(&mut caller, name).unwrap_or(0)
        },
    )?;
    linker.func_wrap(
        "env",
        "F",
        |mut caller: Caller<'_, CmgState>, handle: i32| {
            trace_import("F", &[handle]);
            emval_decref(&mut caller, handle);
        },
    )?;
    linker.func_wrap(
        "env",
        "G",
        |mut caller: Caller<'_, CmgState>,
         handle: i32,
         return_type: i32,
         destructors: i32|
         -> f64 {
            trace_import("G", &[handle, return_type, destructors]);
            emval_as(&mut caller, handle, return_type, destructors).unwrap_or(0) as f64
        },
    )?;
    linker.func_wrap("env", "H", |_fetch: i32| {})?;
    linker.func_wrap(
        "env",
        "I",
        |mut caller: Caller<'_, CmgState>, func: i32, a: i32| -> i32 {
            call_table_i(&mut caller, func, &[a]).unwrap_or(0)
        },
    )?;
    linker.func_wrap("env", "J", |_raw_type: i32, _name: i32| {
        trace_import("J", &[_raw_type, _name]);
    })?;
    linker.func_wrap("env", "K", |_raw_type: i32, _char_size: i32, _name: i32| {
        trace_import("K", &[_raw_type, _char_size, _name]);
    })?;
    linker.func_wrap(
        "env",
        "L",
        |mut caller: Caller<'_, CmgState>, raw_type: i32, name: i32| {
            trace_import("L", &[raw_type, name]);
            if read_c_string(&mut caller, name).ok().as_deref() == Some("std::string") {
                caller.data_mut().emval.std_string_type = Some(raw_type);
            }
        },
    )?;
    linker.func_wrap(
        "env",
        "M",
        |_raw_type: i32, _data_type_index: i32, _name: i32| {
            trace_import("M", &[_raw_type, _data_type_index, _name]);
        },
    )?;
    linker.func_wrap(
        "env",
        "N",
        |_raw_type: i32, _name: i32, _size: i32, _min: i32, _max: i32| {
            trace_import("N", &[_raw_type, _name, _size, _min, _max]);
        },
    )?;
    linker.func_wrap("env", "O", |_raw_type: i32, _name: i32, _size: i32| {
        trace_import("O", &[_raw_type, _name, _size]);
    })?;
    linker.func_wrap("env", "P", |_raw_type: i32, _name: i32| {
        trace_import("P", &[_raw_type, _name]);
    })?;
    linker.func_wrap(
        "env",
        "Q",
        |_raw_type: i32, _name: i32, _size: i32, _true_value: i32, _false_value: i32| {
            trace_import("Q", &[_raw_type, _name, _size, _true_value, _false_value]);
        },
    )?;
    linker.func_wrap("env", "R", |_which: i32, _varargs: i32| -> i32 { 0 })?;
    linker.func_wrap("env", "S", |_buf: i32, _len: i32| -> i32 { 0 })?;
    linker.func_wrap("env", "T", |_which: i32, _varargs: i32| -> i32 { 0 })?;
    linker.func_wrap("env", "U", |_errno: i32| {})?;
    linker.func_wrap("env", "V", || -> i32 { 0 })?;
    linker.func_wrap("env", "W", |_ptr: i32| -> i32 { 0 })?;
    linker.func_wrap(
        "env",
        "X",
        |mut caller: Caller<'_, CmgState>,
         func: i32,
         a: i32,
         b: i32,
         c: i32,
         d: i32,
         e: i32,
         f: i32| {
            let _ = call_table_v(&mut caller, func, &[a, b, c, d, e, f]);
        },
    )?;
    linker.func_wrap(
        "env",
        "Y",
        |mut caller: Caller<'_, CmgState>, func: i32, a: i32, b: i32, c: i32, d: i32, e: i32| {
            let _ = call_table_v(&mut caller, func, &[a, b, c, d, e]);
        },
    )?;
    linker.func_wrap(
        "env",
        "Z",
        |mut caller: Caller<'_, CmgState>, func: i32, a: i32, b: i32, c: i32, d: i32| {
            let _ = call_table_v(&mut caller, func, &[a, b, c, d]);
        },
    )?;
    linker.func_wrap(
        "env",
        "_",
        |mut caller: Caller<'_, CmgState>, func: i32, a: i32, b: i32, c: i32| {
            let _ = call_table_v(&mut caller, func, &[a, b, c]);
        },
    )?;
    linker.func_wrap(
        "env",
        "$",
        |mut caller: Caller<'_, CmgState>, func: i32, a: i32, b: i32| {
            let _ = call_table_v(&mut caller, func, &[a, b]);
        },
    )?;
    linker.func_wrap(
        "env",
        "aa",
        |mut caller: Caller<'_, CmgState>, ptr: i32| -> () {
            let message = read_c_string(&mut caller, ptr).unwrap_or_else(|_| "abort".to_string());
            panic!("CMG abort: {message}");
        },
    )?;
    Ok(())
}

fn trace_import(name: &str, args: &[i32]) {
    if std::env::var_os("CMG_TRACE_IMPORTS").is_some() {
        eprintln!("cmg-import {name} {args:?}");
    }
}

fn trace_stage(stage: &str) {
    if std::env::var_os("CMG_TRACE_STAGE").is_some() {
        eprintln!("cmg-stage {stage}");
    }
}

fn asm_const_ii(caller: &mut Caller<'_, CmgState>, index: i32, arg: i32) -> Result<i32> {
    if index != 0 {
        return Ok(0);
    }
    let expr = read_c_string(caller, arg)?;
    let location = caller.data().location.clone();
    let value = match expr.as_str() {
        "location.href" => location.href.clone(),
        "self.location.href" => location.href.clone(),
        "window.location.href" => location.href.clone(),
        "location.host" => location.host.clone(),
        "self.location.host" => location.host.clone(),
        "window.location.host" => location.host.clone(),
        "location.hostname" => location.hostname.clone(),
        "self.location.hostname" => location.hostname.clone(),
        "window.location.hostname" => location.hostname.clone(),
        "location.origin" => location.origin.clone(),
        "self.location.origin" => location.origin.clone(),
        "window.location.origin" => location.origin.clone(),
        "location.protocol" => location.protocol.clone(),
        "self.location.protocol" => location.protocol.clone(),
        "window.location.protocol" => location.protocol.clone(),
        "document.URL" => location.href.clone(),
        _ => String::new(),
    };
    let before = if std::env::var_os("CMG_TRACE_STAGE").is_some() {
        dynamic_top_from_caller(caller).ok()
    } else {
        None
    };
    let ptr = malloc_c_string(caller, &value)?;
    if std::env::var_os("CMG_TRACE_STAGE").is_some() {
        let after = dynamic_top_from_caller(caller).ok();
        eprintln!(
            "cmg-stage asm_const expr={expr:?} value_len={} ptr={ptr} dyn={before:?}->{after:?}",
            value.len()
        );
    }
    Ok(ptr)
}

fn emscripten_start_fetch(caller: &mut Caller<'_, CmgState>, fetch: i32) -> Result<i32> {
    let memory = cmg_memory(caller)?;
    let (url, onsuccess, onerror, flags) = {
        let data = memory.data(&mut *caller);
        let url_ptr = read_u32_le_raw(data, fetch + 8)? as i32;
        let attr = fetch + 112;
        (
            read_c_string_from_memory(data, url_ptr)?,
            read_u32_le_raw(data, attr + 36)? as i32,
            read_u32_le_raw(data, attr + 40)? as i32,
            read_u32_le_raw(data, attr + 52)?,
        )
    };
    let handle = {
        let state = caller.data_mut();
        let handle = state.next_fetch_handle.max(1);
        state.next_fetch_handle = handle + 1;
        state.pending_fetches.push(PendingFetch {
            ptr: fetch,
            url: url.clone(),
            onsuccess,
            onerror,
            flags,
        });
        handle
    };
    {
        let data = memory.data_mut(caller);
        write_i32_le_raw(data, fetch, handle)?;
    }
    if std::env::var_os("CMG_TRACE_STAGE").is_some() {
        eprintln!(
            "cmg-stage fetch queued ptr={fetch} handle={handle} url={url} flags={flags:#x} onsuccess={onsuccess} onerror={onerror}"
        );
    }
    Ok(fetch)
}

fn emval_register(caller: &mut Caller<'_, CmgState>, value: EmvalValue) -> i32 {
    match value {
        EmvalValue::Undefined => 1,
        EmvalValue::Null => 2,
        EmvalValue::Bool(true) => 3,
        EmvalValue::Bool(false) => 4,
        other => {
            let state = &mut caller.data_mut().emval;
            if let Some(handle) = state.free.pop() {
                let idx = handle as usize;
                if idx >= state.handles.len() {
                    state.handles.resize(idx + 1, None);
                }
                state.handles[idx] = Some(other);
                handle
            } else {
                state.handles.push(Some(other));
                (state.handles.len() - 1) as i32
            }
        }
    }
}

fn emval_value(caller: &Caller<'_, CmgState>, handle: i32) -> Option<EmvalValue> {
    caller
        .data()
        .emval
        .handles
        .get(handle as usize)
        .and_then(Clone::clone)
}

fn emval_decref(caller: &mut Caller<'_, CmgState>, handle: i32) {
    if handle <= 4 {
        return;
    }
    let state = &mut caller.data_mut().emval;
    let idx = handle as usize;
    if idx < state.handles.len() && state.handles[idx].is_some() {
        state.handles[idx] = None;
        state.free.push(handle);
        if state.location_handle == Some(handle) {
            state.location_handle = None;
        }
    }
}

fn emval_get_global(caller: &mut Caller<'_, CmgState>, name_ptr: i32) -> Result<i32> {
    let name = if name_ptr == 0 {
        String::new()
    } else {
        read_c_string(caller, name_ptr)?
    };
    if name == "location" {
        if let Some(handle) = caller.data().emval.location_handle {
            if emval_value(caller, handle).is_some() {
                return Ok(handle);
            }
        }
        let handle = emval_register(caller, EmvalValue::Location);
        caller.data_mut().emval.location_handle = Some(handle);
        return Ok(handle);
    }
    Ok(emval_register(caller, EmvalValue::Undefined))
}

fn emval_take_value(caller: &mut Caller<'_, CmgState>, type_id: i32, ptr: i32) -> Result<i32> {
    let value = if caller.data().emval.std_string_type == Some(type_id) {
        read_std_string(caller, ptr)?
    } else {
        String::new()
    };
    Ok(emval_register(caller, EmvalValue::String(value)))
}

fn emval_get_property(caller: &mut Caller<'_, CmgState>, obj: i32, prop: i32) -> Result<i32> {
    let object = emval_value(caller, obj).unwrap_or(EmvalValue::Undefined);
    let property = emval_value(caller, prop).unwrap_or(EmvalValue::Undefined);
    let value = match (object, property) {
        (EmvalValue::Location, EmvalValue::String(name)) => match name.as_str() {
            "host" => EmvalValue::String(caller.data().location.host.clone()),
            "protocol" => EmvalValue::String(caller.data().location.protocol.clone()),
            "href" => EmvalValue::String(caller.data().location.href.clone()),
            "hostname" => EmvalValue::String(caller.data().location.hostname.clone()),
            "origin" => EmvalValue::String(caller.data().location.origin.clone()),
            _ => EmvalValue::Undefined,
        },
        _ => EmvalValue::Undefined,
    };
    Ok(emval_register(caller, value))
}

fn emval_as(
    caller: &mut Caller<'_, CmgState>,
    handle: i32,
    return_type: i32,
    destructors_ptr: i32,
) -> Result<i32> {
    if caller.data().emval.std_string_type != Some(return_type) {
        return Ok(0);
    }
    let value = match emval_value(caller, handle).unwrap_or(EmvalValue::Undefined) {
        EmvalValue::String(value) => value,
        EmvalValue::Bool(value) => {
            if value {
                "true".to_string()
            } else {
                "false".to_string()
            }
        }
        EmvalValue::Null => "null".to_string(),
        EmvalValue::Undefined => String::new(),
        EmvalValue::Location => "location".to_string(),
        EmvalValue::Destructors(_) => String::new(),
    };
    let wire = malloc_std_string(caller, &value)?;
    let destructors = emval_register(caller, EmvalValue::Destructors(vec![wire]));
    write_i32(caller, destructors_ptr, destructors)?;
    if std::env::var_os("CMG_TRACE_EMVAL").is_some() {
        eprintln!(
            "cmg-emval as handle={handle} return_type={return_type} value={value:?} wire={wire} destructors={destructors}"
        );
    }
    Ok(wire)
}

fn emval_run_destructors(caller: &mut Caller<'_, CmgState>, destructors: i32) {
    if let Some(EmvalValue::Destructors(ptrs)) = emval_value(caller, destructors) {
        for ptr in ptrs {
            if std::env::var_os("CMG_TRACE_EMVAL").is_some() {
                eprintln!("cmg-emval free destructor={destructors} ptr={ptr}");
            }
            let _ = free_std_string(caller, ptr);
        }
    }
    emval_decref(caller, destructors);
}

fn read_std_string(caller: &mut Caller<'_, CmgState>, ptr: i32) -> Result<String> {
    let memory = cmg_memory(caller)?;
    let (string_ptr, bytes) = {
        let data = memory.data(&mut *caller);
        let string_ptr = read_u32_le_raw(data, ptr)? as i32;
        let len = read_u32_le_raw(data, string_ptr)? as usize;
        let start = string_ptr as usize + 4;
        let end = start + len;
        let bytes = data
            .get(start..end)
            .ok_or_else(|| {
                anyhow!(
                    "CMG std::string read out of range ptr={ptr} string_ptr={string_ptr} len={len}"
                )
            })?
            .to_vec();
        (string_ptr, bytes)
    };
    let value = String::from_utf8(bytes).context("CMG std::string is not utf8")?;
    free_std_string(caller, string_ptr)?;
    Ok(value)
}

fn malloc_std_string(caller: &mut Caller<'_, CmgState>, value: &str) -> Result<i32> {
    let malloc = caller
        .data()
        .malloc
        .clone()
        .ok_or_else(|| anyhow!("CMG malloc export missing"))?;
    let ptr = malloc.call(&mut *caller, (4 + value.len() + 1) as i32)?;
    let memory = cmg_memory(caller)?;
    let data = memory.data_mut(caller);
    write_i32_le_raw(data, ptr, value.len() as i32)?;
    let start = ptr as usize + 4;
    data.get_mut(start..start + value.len())
        .ok_or_else(|| anyhow!("CMG malloc std::string write out of range"))?
        .copy_from_slice(value.as_bytes());
    data[start + value.len()] = 0;
    if std::env::var_os("CMG_TRACE_EMVAL").is_some() {
        eprintln!(
            "cmg-emval malloc_std_string ptr={ptr} len={} value={value:?}",
            value.len()
        );
    }
    Ok(ptr)
}

fn free_std_string(caller: &mut Caller<'_, CmgState>, ptr: i32) -> Result<()> {
    if ptr <= 0 {
        return Ok(());
    }
    let free = caller
        .data()
        .free
        .clone()
        .ok_or_else(|| anyhow!("CMG free export missing"))?;
    if std::env::var_os("CMG_TRACE_EMVAL").is_some() {
        eprintln!("cmg-emval free_std_string ptr={ptr}");
    }
    free.call(caller, ptr)?;
    Ok(())
}

fn call_table_i(caller: &mut Caller<'_, CmgState>, func_index: i32, args: &[i32]) -> Result<i32> {
    let table = caller
        .data()
        .table
        .ok_or_else(|| anyhow!("CMG table missing"))?;
    let idx = resolve_func_index(caller, func_index)?;
    let Some(Ref::Func(Some(func))) = table.get(&mut *caller, idx as u64) else {
        return Ok(0);
    };
    match args.len() {
        1 => {
            let f = func.typed::<i32, i32>(&caller)?;
            Ok(f.call(caller, args[0])?)
        }
        2 => {
            let f = func.typed::<(i32, i32), i32>(&caller)?;
            Ok(f.call(caller, (args[0], args[1]))?)
        }
        3 => {
            let f = func.typed::<(i32, i32, i32), i32>(&caller)?;
            Ok(f.call(caller, (args[0], args[1], args[2]))?)
        }
        _ => Ok(0),
    }
}

fn call_table_v(caller: &mut Caller<'_, CmgState>, func_index: i32, args: &[i32]) -> Result<()> {
    let table = caller
        .data()
        .table
        .ok_or_else(|| anyhow!("CMG table missing"))?;
    let idx = resolve_func_index(caller, func_index)?;
    let Some(Ref::Func(Some(func))) = table.get(&mut *caller, idx as u64) else {
        return Ok(());
    };
    match args.len() {
        0 => {
            let f = func.typed::<(), ()>(&caller)?;
            f.call(caller, ())?;
        }
        1 => {
            let f = func.typed::<i32, ()>(&caller)?;
            f.call(caller, args[0])?;
        }
        2 => {
            let f = func.typed::<(i32, i32), ()>(&caller)?;
            f.call(caller, (args[0], args[1]))?;
        }
        3 => {
            let f = func.typed::<(i32, i32, i32), ()>(&caller)?;
            f.call(caller, (args[0], args[1], args[2]))?;
        }
        4 => {
            let f = func.typed::<(i32, i32, i32, i32), ()>(&caller)?;
            f.call(caller, (args[0], args[1], args[2], args[3]))?;
        }
        5 => {
            let f = func.typed::<(i32, i32, i32, i32, i32), ()>(&caller)?;
            f.call(caller, (args[0], args[1], args[2], args[3], args[4]))?;
        }
        6 => {
            let f = func.typed::<(i32, i32, i32, i32, i32, i32), ()>(&caller)?;
            f.call(
                caller,
                (args[0], args[1], args[2], args[3], args[4], args[5]),
            )?;
        }
        _ => {}
    }
    Ok(())
}

fn resolve_func_index(caller: &Caller<'_, CmgState>, func_index: i32) -> Result<u32> {
    Ok(resolve_func_index_from_state(caller.data(), func_index))
}

fn resolve_func_index_from_state(state: &CmgState, func_index: i32) -> u32 {
    if func_index >= 1 {
        let idx = (func_index - 1) as usize;
        if idx < state.function_pointers.len() {
            if let Some(value) = state.function_pointers[idx] {
                return value;
            }
        }
    }
    func_index as u32
}

fn resize_heap(caller: &mut Caller<'_, CmgState>, requested_size: i32) -> Result<i32> {
    let memory = caller
        .data()
        .memory
        .ok_or_else(|| anyhow!("CMG memory missing"))?;
    let current = memory.data_size(&mut *caller);
    if requested_size as usize <= current {
        return Ok(1);
    }
    let page = 65_536usize;
    let wanted_pages = (requested_size as usize + page - 1) / page;
    let current_pages = current / page;
    if wanted_pages > current_pages {
        memory.grow(caller, (wanted_pages - current_pages) as u64)?;
    }
    Ok(1)
}

fn malloc_c_string(caller: &mut Caller<'_, CmgState>, value: &str) -> Result<i32> {
    let len = value.len() + 1;
    let malloc = caller
        .data()
        .malloc
        .clone()
        .ok_or_else(|| anyhow!("CMG malloc export missing"))?;
    let ptr = malloc.call(&mut *caller, len as i32)?;
    let memory = cmg_memory(caller)?;
    let data = memory.data_mut(caller);
    let start = ptr as usize;
    data.get_mut(start..start + value.len())
        .ok_or_else(|| anyhow!("CMG malloc c-string allocation out of range"))?
        .copy_from_slice(value.as_bytes());
    data[start + value.len()] = 0;
    Ok(ptr)
}

fn dynamic_top_from_caller(caller: &mut Caller<'_, CmgState>) -> Result<u32> {
    let memory = cmg_memory(caller)?;
    let data = memory.data(caller);
    read_u32_le_raw(data, DYNAMICTOP_PTR)
}

fn log_from_wasm(caller: &mut Caller<'_, CmgState>, _flags: i32, _varargs: i32) {
    let _ = caller;
}

fn cmg_memory(caller: &mut Caller<'_, CmgState>) -> Result<Memory> {
    caller
        .data()
        .memory
        .ok_or_else(|| anyhow!("CMG memory not initialized"))
}

fn write_i32(caller: &mut Caller<'_, CmgState>, ptr: i32, value: i32) -> Result<()> {
    let memory = cmg_memory(caller)?;
    let data = memory.data_mut(caller);
    write_i32_le_raw(data, ptr, value)
}

fn write_i32_le_raw(data: &mut [u8], ptr: i32, value: i32) -> Result<()> {
    let start = ptr as usize;
    data.get_mut(start..start + 4)
        .ok_or_else(|| anyhow!("CMG i32 write out of range ptr={ptr}"))?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn write_u16_le_raw(data: &mut [u8], ptr: i32, value: u16) -> Result<()> {
    let start = ptr as usize;
    data.get_mut(start..start + 2)
        .ok_or_else(|| anyhow!("CMG u16 write out of range ptr={ptr}"))?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn write_u64_le_raw(data: &mut [u8], ptr: i32, value: u64) -> Result<()> {
    let start = ptr as usize;
    data.get_mut(start..start + 8)
        .ok_or_else(|| anyhow!("CMG u64 write out of range ptr={ptr}"))?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn read_u32_le_raw(data: &[u8], ptr: i32) -> Result<u32> {
    let start = ptr as usize;
    let bytes = data
        .get(start..start + 4)
        .ok_or_else(|| anyhow!("CMG u32 read out of range ptr={ptr}"))?;
    Ok(u32::from_le_bytes(
        bytes.try_into().expect("slice has four bytes"),
    ))
}

fn write_c_string_raw(data: &mut [u8], ptr: i32, max_len: usize, value: &str) -> Result<()> {
    let start = ptr as usize;
    let end = start + max_len;
    let target = data
        .get_mut(start..end)
        .ok_or_else(|| anyhow!("CMG c-string raw write out of range ptr={ptr} len={max_len}"))?;
    target.fill(0);
    let len = value.len().min(max_len.saturating_sub(1));
    target[..len].copy_from_slice(&value.as_bytes()[..len]);
    Ok(())
}

fn copy_within_memory(
    caller: &mut Caller<'_, CmgState>,
    dst: i32,
    src: i32,
    len: i32,
) -> Result<()> {
    if len <= 0 {
        return Ok(());
    }
    let memory = cmg_memory(caller)?;
    let data = memory.data_mut(caller);
    let src_start = src as usize;
    let src_end = src_start + len as usize;
    let dst_start = dst as usize;
    data.copy_within(src_start..src_end, dst_start);
    Ok(())
}

fn read_c_string(caller: &mut Caller<'_, CmgState>, ptr: i32) -> Result<String> {
    if ptr == 0 {
        return Ok(String::new());
    }
    let memory = cmg_memory(caller)?;
    let data = memory.data(caller);
    read_c_string_from_memory(data, ptr)
}

fn read_c_string_from_memory(data: &[u8], ptr: i32) -> Result<String> {
    if ptr == 0 {
        return Ok(String::new());
    }
    let start = ptr as usize;
    let mut end = start;
    while end < data.len() && data[end] != 0 {
        end += 1;
    }
    String::from_utf8(data[start..end].to_vec()).context("CMG c-string is not utf8")
}

fn now_duration() -> std::time::Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
}

fn now_ms_f64() -> f64 {
    now_duration().as_millis() as f64
}

fn frozen_now_ms() -> Option<f64> {
    std::env::var("CMG_FROZEN_NOW_MS").ok()?.parse::<f64>().ok()
}

fn cmg_now_ms(state: &CmgState) -> f64 {
    state.frozen_now_ms.unwrap_or_else(now_ms_f64)
}

fn cmg_live8_only() -> bool {
    std::env::var_os("CMG_LIVE8_ONLY").is_some()
}

fn trace_live_input_len_matches(len: usize) -> bool {
    let Some(value) = std::env::var("CMG_TRACE_LIVE_INPUT_LEN").ok() else {
        return false;
    };
    value == "*"
        || value
            .split(',')
            .any(|entry| entry.trim().parse::<usize>().ok() == Some(len))
}

fn trace_live_call_matches(call: usize) -> bool {
    let Some(value) = std::env::var("CMG_TRACE_LIVE_CALL").ok() else {
        return false;
    };
    value == "*"
        || value.split(',').any(|entry| {
            let entry = entry.trim();
            if let Some((start, end)) = entry.split_once("..=") {
                return start.trim().parse::<usize>().ok() <= Some(call)
                    && end.trim().parse::<usize>().ok() >= Some(call);
            }
            if let Some((start, end)) = entry.split_once("..") {
                return start.trim().parse::<usize>().ok() <= Some(call)
                    && end
                        .trim()
                        .parse::<usize>()
                        .ok()
                        .is_some_and(|end| call < end);
            }
            entry.parse::<usize>().ok() == Some(call)
        })
}

fn hex_head(bytes: &[u8], max: usize) -> String {
    hex::encode(&bytes[..bytes.len().min(max)])
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIRST_BIG_INPUT: &[u8] =
        include_bytes!("../../run/rust-verify/browser-first-big-live8-input.bin");
    #[test]
    fn cmg_module_dec_live_changes_browser_first_big_prefix() {
        let mut cmg = CmgRuntime::load().unwrap();
        let _ = cmg.prime("1778300291980").unwrap();
        let _ = cmg.update("1778300291980").unwrap();
        let out = cmg
            .module_dec_live(
                "1778300291980",
                FIRST_BIG_INPUT,
                "https://www.yangshipin.cn",
            )
            .unwrap();
        assert!(out.len() <= FIRST_BIG_INPUT.len());
        assert_ne!(out.as_slice(), FIRST_BIG_INPUT);
    }

    #[test]
    fn cmg_debug_probe_when_requested() {
        let Some(media_tag_id) = std::env::var("CMG_DEBUG_MEDIA_TAG_ID").ok() else {
            return;
        };
        let page_url = std::env::var("CMG_DEBUG_PAGE_URL")
            .unwrap_or_else(|_| "https://www.yangshipin.cn/tv/home?pid=600001859".to_string());
        let mut cmg = CmgRuntime::load_for_page(&page_url).unwrap();
        let init = cmg.prime(&media_tag_id).unwrap();
        eprintln!("cmg-debug init dec={init} hex={init:08x}");
        for index in 0..5 {
            let update = cmg.update(&media_tag_id).unwrap();
            eprintln!("cmg-debug update#{index} dec={update} hex={update:08x}");
        }
    }
}
