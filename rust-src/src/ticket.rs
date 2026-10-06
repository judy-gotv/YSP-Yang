use anyhow::{anyhow, Result};
use wasmtime::{Caller, Engine, Linker, Memory, Ref, Store, Table};

use crate::{
    assets::TICKET_WASM,
    constants::{APP_VER, YSPAPPID},
};

struct TicketState {
    memory: Option<Memory>,
}

pub fn build_ticket(pid: &str, auth_ts: &str, cnlid: &str, guid: &str) -> Result<String> {
    let mut runtime = TicketRuntime::load()?;
    runtime.encrypt(pid, auth_ts, cnlid, guid, YSPAPPID, APP_VER)
}

struct TicketRuntime {
    store: Store<TicketState>,
    memory: Memory,
    malloc: wasmtime::TypedFunc<i32, i32>,
    encrypt: wasmtime::TypedFunc<(i32, i32, i32, i32, i32, i32, i32), ()>,
}

impl TicketRuntime {
    fn load() -> Result<Self> {
        let engine = Engine::default();
        let module = wasmtime::Module::from_binary(&engine, TICKET_WASM)?;
        let mut store = Store::new(&engine, TicketState { memory: None });
        let memory = Memory::new(&mut store, wasmtime::MemoryType::new(256, Some(256)))?;
        let table = Table::new(
            &mut store,
            wasmtime::TableType::new(wasmtime::RefType::FUNCREF, 743, Some(743)),
            Ref::Func(None),
        )?;
        store.data_mut().memory = Some(memory);

        let mut linker = Linker::new(&engine);
        linker.define(&mut store, "a", "a", memory)?;
        linker.define(&mut store, "a", "b", table)?;
        define_ticket_imports(&mut linker)?;

        let instance = linker.instantiate(&mut store, &module)?;
        let ctor = instance.get_typed_func::<(), ()>(&mut store, "P")?;
        ctor.call(&mut store, ())?;
        let malloc = instance.get_typed_func::<i32, i32>(&mut store, "R")?;
        let encrypt =
            instance.get_typed_func::<(i32, i32, i32, i32, i32, i32, i32), ()>(&mut store, "T")?;
        Ok(Self {
            store,
            memory,
            malloc,
            encrypt,
        })
    }

    fn encrypt(
        &mut self,
        pid: &str,
        auth_ts: &str,
        cnlid: &str,
        guid: &str,
        yspappid: &str,
        app_ver: &str,
    ) -> Result<String> {
        let values = [
            self.alloc_c_string(pid)?,
            self.alloc_c_string(auth_ts)?,
            self.alloc_c_string(cnlid)?,
            self.alloc_c_string(guid)?,
            self.alloc_c_string(yspappid)?,
            self.alloc_c_string(app_ver)?,
        ];
        let out_len = pid.len() + auth_ts.len() + guid.len() + yspappid.len() + 14;
        let out_ptr = self.malloc.call(&mut self.store, out_len as i32)?;
        self.encrypt.call(
            &mut self.store,
            (
                values[0], values[1], values[2], values[3], values[4], values[5], out_ptr,
            ),
        )?;
        let data = self.memory.data(&self.store);
        let bytes = data
            .get(out_ptr as usize..out_ptr as usize + out_len)
            .ok_or_else(|| anyhow!("ticket output out of range"))?;
        Ok(hex::encode(bytes))
    }

    fn alloc_c_string(&mut self, value: &str) -> Result<i32> {
        let bytes = value.as_bytes();
        let ptr = self
            .malloc
            .call(&mut self.store, (bytes.len() + 1) as i32)?;
        let data = self.memory.data_mut(&mut self.store);
        let start = ptr as usize;
        let end = start + bytes.len();
        data.get_mut(start..end)
            .ok_or_else(|| anyhow!("ticket string allocation out of range"))?
            .copy_from_slice(bytes);
        data[end] = 0;
        Ok(ptr)
    }
}

fn define_ticket_imports(linker: &mut Linker<TicketState>) -> Result<()> {
    linker.func_wrap("a", "c", |_sig: i32, _act: i32, _old: i32| -> i32 { 0 })?;
    linker.func_wrap(
        "a",
        "d",
        |mut caller: Caller<'_, TicketState>, ptr: i32| -> i32 {
            let now = now_secs_i32();
            if ptr != 0 {
                let _ = write_i32(&mut caller, ptr, now);
            }
            now
        },
    )?;
    linker.func_wrap("a", "e", || -> i32 { 42 })?;
    linker.func_wrap("a", "f", |_fd: i32| -> i32 { 0 })?;
    linker.func_wrap("a", "g", |_handle: i32| -> i32 { 0 })?;
    linker.func_wrap("a", "h", || -> i32 { 0 })?;
    linker.func_wrap(
        "a",
        "i",
        |mut caller: Caller<'_, TicketState>, ptr: i32, _tz: i32| -> i32 {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            let _ = write_i32(&mut caller, ptr, now.as_secs() as i32);
            let _ = write_i32(&mut caller, ptr + 4, now.subsec_micros() as i32);
            0
        },
    )?;
    linker.func_wrap(
        "a",
        "j",
        |mut caller: Caller<'_, TicketState>, clock_id: i32, tp: i32| -> i32 {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            if clock_id == 0 || clock_id == 1 || clock_id == 4 {
                let _ = write_i32(&mut caller, tp, now.as_secs() as i32);
                let _ = write_i32(&mut caller, tp + 4, now.subsec_nanos() as i32);
                0
            } else {
                -1
            }
        },
    )?;
    linker.func_wrap("a", "k", |_handle: i32, _symbol: i32| -> i32 { 0 })?;
    linker.func_wrap("a", "l", |_filename: i32, _flag: i32| -> i32 { 0 })?;
    linker.func_wrap(
        "a",
        "m",
        |_fd: i32, _iov: i32, _iovcnt: i32, pnum: i32| -> i32 {
            let _ = pnum;
            0
        },
    )?;
    linker.func_wrap("a", "n", |_fd: i32, _cmd: i32, _varargs: i32| -> i32 { 0 })?;
    linker.func_wrap("a", "o", |_path: i32, _buf: i32| -> i32 { -1 })?;
    linker.func_wrap("a", "p", |_fd: i32, _op: i32, _varargs: i32| -> i32 { 0 })?;
    linker.func_wrap("a", "q", |_path: i32, _flags: i32, _varargs: i32| -> i32 {
        -1
    })?;
    linker.func_wrap("a", "r", |_addr: i32, _info: i32| -> i32 { 0 })?;
    linker.func_wrap(
        "a",
        "s",
        |mut caller: Caller<'_, TicketState>,
         _fd: i32,
         _low: i32,
         _high: i32,
         _whence: i32,
         out: i32|
         -> i32 {
            let _ = write_i32(&mut caller, out, 0);
            let _ = write_i32(&mut caller, out + 4, 0);
            0
        },
    )?;
    linker.func_wrap(
        "a",
        "t",
        |mut caller: Caller<'_, TicketState>, dst: i32, src: i32, len: i32| -> i32 {
            let _ = copy_within_memory(&mut caller, dst, src, len);
            dst
        },
    )?;
    linker.func_wrap("a", "u", |_requested_size: i32| -> i32 { 0 })?;
    linker.func_wrap(
        "a",
        "v",
        |mut caller: Caller<'_, TicketState>, _fd: i32, pbuf: i32| -> i32 {
            let _ = write_i8(&mut caller, pbuf, 4);
            0
        },
    )?;
    linker.func_wrap("a", "w", |_environ: i32, _environ_buf: i32| -> i32 { 0 })?;
    linker.func_wrap(
        "a",
        "x",
        |mut caller: Caller<'_, TicketState>, count: i32, buf_size: i32| -> i32 {
            let _ = write_i32(&mut caller, count, 0);
            let _ = write_i32(&mut caller, buf_size, 0);
            0
        },
    )?;
    linker.func_wrap("a", "y", |_fd: i32, _buf: i32, _count: i32| -> i32 { 0 })?;
    linker.func_wrap("a", "z", || -> i32 { 0 })?;
    linker.func_wrap("a", "A", || -> i32 { 0 })?;
    linker.func_wrap("a", "B", || -> i32 { 0 })?;
    linker.func_wrap("a", "C", || -> i32 { 0 })?;
    linker.func_wrap(
        "a",
        "D",
        |_fd: i32, _iov: i32, _iovcnt: i32, pnum: i32| -> i32 {
            let _ = pnum;
            0
        },
    )?;
    linker.func_wrap("a", "E", |_fd: i32, _buf: i32| -> i32 { 0 })?;
    linker.func_wrap("a", "F", |_addr: i32, _len: i32| -> i32 { 0 })?;
    linker.func_wrap("a", "G", || {})?;
    linker.func_wrap("a", "H", |_fd: i32, _dirp: i32, _count: i32| -> i32 { 0 })?;
    linker.func_wrap("a", "I", |_sig: i32, _func: i32| -> i32 { 0 })?;
    linker.func_wrap("a", "J", |_ptr: i32| -> i32 { 0 })?;
    linker.func_wrap("a", "K", |_ptr: i32| -> i32 { 0 })?;
    linker.func_wrap("a", "L", |_ptr: i32| -> i32 { 0 })?;
    linker.func_wrap("a", "M", |_ptr: i32| -> i32 { 0 })?;
    linker.func_wrap("a", "N", |_ptr: i32, _attr: i32| -> i32 { 0 })?;
    // The official loader runs this WASM in a browser-like context. Export T
    // checks this import before entering the AES branch; returning 0 leaves
    // the caller-provided output buffer untouched.
    linker.func_wrap("a", "O", || -> i32 { 1 })?;
    Ok(())
}

fn ticket_memory(caller: &mut Caller<'_, TicketState>) -> Result<Memory> {
    caller
        .data()
        .memory
        .ok_or_else(|| anyhow!("ticket memory not initialized"))
}

fn write_i8(caller: &mut Caller<'_, TicketState>, ptr: i32, value: u8) -> Result<()> {
    let memory = ticket_memory(caller)?;
    let data = memory.data_mut(caller);
    data.get_mut(ptr as usize)
        .ok_or_else(|| anyhow!("ticket i8 write out of range"))
        .map(|slot| *slot = value)
}

fn write_i32(caller: &mut Caller<'_, TicketState>, ptr: i32, value: i32) -> Result<()> {
    let memory = ticket_memory(caller)?;
    let data = memory.data_mut(caller);
    data.get_mut(ptr as usize..ptr as usize + 4)
        .ok_or_else(|| anyhow!("ticket i32 write out of range"))?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn copy_within_memory(
    caller: &mut Caller<'_, TicketState>,
    dst: i32,
    src: i32,
    len: i32,
) -> Result<()> {
    if len <= 0 {
        return Ok(());
    }
    let memory = ticket_memory(caller)?;
    let data = memory.data_mut(caller);
    let src_start = src as usize;
    let src_end = src_start + len as usize;
    let dst_start = dst as usize;
    data.copy_within(src_start..src_end, dst_start);
    Ok(())
}

fn now_secs_i32() -> i32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticket_matches_browser_shape_for_sample() {
        let ticket = build_ticket(
            "600099502",
            "1778337597",
            "2027249301",
            "moygaemw_oj9xhxuw53",
        )
        .unwrap();
        println!("ticket_sample={ticket}");
        assert_eq!(ticket.len(), 122);
        assert!(ticket.chars().all(|ch| ch.is_ascii_hexdigit()));
        assert_ne!(ticket, "0".repeat(122));
        assert!(ticket.starts_with("5c40d99c3945f9087e0e99baca1a22edfc53707ceceed2181f99325fc5b5a3ffad9c8dd30779f69718b282fe1cb2211ec0e6cc"));
    }
}
