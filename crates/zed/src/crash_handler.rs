//! Central crash/panic interception for Windows.
//!
//! Zed can exit silently (no console, no panic output) when it crashes during early startup. This
//! module installs two hooks, both writing diagnostics to `<temp>/zed_crash.log`:
//!
//! 1. A Rust panic hook that logs the panic message and a full backtrace.
//! 2. An `SetUnhandledExceptionFilter` for native exceptions (e.g. access violations) that
//!    symbolises the captured stack via dbghelp.
//!
//! Call [`install`] as early as possible in `main()`.

#![cfg(target_os = "windows")]

use std::ffi::c_void;

use windows::core::PCSTR;
use windows::Win32::{
    Foundation::HANDLE,
    System::{
        Diagnostics::Debug::{
            EXCEPTION_POINTERS, IMAGEHLP_LINE64, IMAGEHLP_MODULE64, SYMBOL_INFO,
            RtlCaptureStackBackTrace, SetUnhandledExceptionFilter, SymCleanup, SymFromAddr,
            SymGetLineFromAddr64, SymGetModuleInfo64, SymInitialize,
        },
        Threading::GetCurrentProcess,
    },
};

pub fn install() {
    install_exception_filter();
    install_panic_hook();
}

fn install_panic_hook() {
    std::panic::set_hook(Box::new(|panic_info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        let message = format!("Zed panicked: {panic_info}\n\nStack backtrace:\n{backtrace}\n");
        log::error!("{message}");
        write_crash_file(&message);
    }));
}

fn install_exception_filter() {
    unsafe extern "system" fn filter(exception_info: *const EXCEPTION_POINTERS) -> i32 {
        let code = if exception_info.is_null() {
            0
        } else {
            unsafe { (*(*exception_info).ExceptionRecord).ExceptionCode.0 as u32 }
        };

        let mut frames: [*mut c_void; 64] = [std::ptr::null_mut(); 64];
        let count = unsafe { RtlCaptureStackBackTrace(2, &mut frames, None) } as usize;

        let process = unsafe { GetCurrentProcess() };
        let _ = unsafe { SymInitialize(process, PCSTR::null(), true) };

        let mut out = String::from("Zed unhandled exception");
        if code != 0 {
            out.push_str(&format!(" (code 0x{code:08X})"));
        }
        out.push('\n');
        for i in 0..count {
            let addr = frames[i] as u64;
            let module = unsafe { symbol_module(process, addr) };
            let name = unsafe { symbol_name(process, addr) };
            let line = unsafe { symbol_line(process, addr) };
            let separator = if module.is_empty() && name.is_empty() {
                String::new()
            } else if name.is_empty() {
                format!("  in {module}")
            } else {
                format!("  {module}!{name}")
            };
            let line = if line.is_empty() {
                String::new()
            } else {
                format!(" ({line})")
            };
            out.push_str(&format!("  {addr:#018x}{separator}{line}\n"));
        }
        let _ = unsafe { SymCleanup(process) };

        write_crash_file(&out);
        // EXCEPTION_CONTINUE_SEARCH: let the default handler terminate the process afterwards.
        -1
    }

    unsafe {
        SetUnhandledExceptionFilter(Some(filter));
    }
}

unsafe fn symbol_name(process: HANDLE, address: u64) -> String {
    const NAME_CAPACITY: usize = 512;
    unsafe {
        let mut buffer = vec![0u8; std::mem::size_of::<SYMBOL_INFO>() + NAME_CAPACITY];
        let symbol = buffer.as_mut_ptr() as *mut SYMBOL_INFO;
        (*symbol).SizeOfStruct = std::mem::size_of::<SYMBOL_INFO>() as u32;
        (*symbol).MaxNameLen = NAME_CAPACITY as u32;
        let mut displacement = 0u64;
        if SymFromAddr(process, address, Some(&mut displacement), symbol).is_ok() {
            let name_len = (*symbol).NameLen as usize;
            let name_ptr = (*symbol).Name.as_ptr() as *const u8;
            let bytes = std::slice::from_raw_parts(name_ptr, name_len);
            return String::from_utf8_lossy(bytes).into_owned();
        }
        String::new()
    }
}

unsafe fn symbol_line(process: HANDLE, address: u64) -> String {
    let mut line = IMAGEHLP_LINE64::default();
    line.SizeOfStruct = std::mem::size_of::<IMAGEHLP_LINE64>() as u32;
    let mut displacement = 0u32;
    if unsafe { SymGetLineFromAddr64(process, address, &mut displacement, &mut line) }.is_ok() {
        let file = unsafe { line.FileName.to_string() }.unwrap_or_default();
        return format!("{file}:{}", line.LineNumber);
    }
    String::new()
}

unsafe fn symbol_module(process: HANDLE, address: u64) -> String {
    let mut module = IMAGEHLP_MODULE64::default();
    module.SizeOfStruct = std::mem::size_of::<IMAGEHLP_MODULE64>() as u32;
    if unsafe { SymGetModuleInfo64(process, address, &mut module) }.is_ok() {
        let name = &module.ModuleName;
        let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        let bytes: Vec<u8> = name[..len].iter().map(|&c| c as u8).collect();
        return String::from_utf8_lossy(&bytes).into_owned();
    }
    String::new()
}

fn write_crash_file(contents: &str) {
    use std::io::Write;
    let path = std::env::temp_dir().join("zed_crash.log");
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(file, "---- {ts} ----");
        let _ = file.write_all(contents.as_bytes());
    }
}