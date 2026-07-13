//! Daemon logging + crash capture.
//!
//! Under a service manager the daemon has no console, so `tracing`'s default
//! stderr sink goes nowhere — which is exactly why the recurring Windows crashes
//! (`0xc0000409`, a Rust panic → `abort`) left no trace of *why*. This module
//! gives the daemon its own on-disk log in a privileged-writable location and,
//! crucially, a panic hook that writes the panic message + location + backtrace
//! **synchronously** to a crash log before the process aborts (the async,
//! non-blocking tracing writer can lose its buffer when the process fastfails).
//!
//! Layout (override the directory with `NULLGATE_LOG_DIR`):
//!   * Windows: `%ProgramData%\Nullgate\logs`
//!   * Linux:   `/var/log/nullgate`
//!   * macOS:   `/Library/Logs/Nullgate`
//!
//! Falls back to `<data_dir>/logs` and finally the temp dir if the preferred
//! location can't be created (e.g. an unprivileged foreground dev run).

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{fmt, EnvFilter};

/// Base name of the rolling daily log and the synchronous crash log.
const LOG_STEM: &str = "nullgate-daemon";
const CRASH_LOG: &str = "nullgate-daemon-crash.log";

/// The preferred, privileged-writable log directory for this platform.
fn preferred_log_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("NULLGATE_LOG_DIR") {
        return PathBuf::from(dir);
    }
    #[cfg(windows)]
    {
        let base = std::env::var_os("ProgramData")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
        base.join("Nullgate").join("logs")
    }
    #[cfg(target_os = "macos")]
    {
        PathBuf::from("/Library/Logs/Nullgate")
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        PathBuf::from("/var/log/nullgate")
    }
}

/// Resolve a writable log directory, trying the privileged location first and
/// falling back to `<data_dir>/logs`, then the temp dir. Always returns a dir we
/// could actually create *and write to*.
fn resolve_log_dir(data_dir: &Path) -> PathBuf {
    let candidates = [
        preferred_log_dir(),
        data_dir.join("logs"),
        std::env::temp_dir().join("nullgate").join("logs"),
    ];
    for dir in candidates.iter() {
        if fs::create_dir_all(dir).is_ok() && is_writable(dir) {
            return dir.clone();
        }
    }
    // Last resort: the temp dir itself (create_dir_all of it is a no-op).
    std::env::temp_dir()
}

/// Whether we can actually create a file in `dir`.
///
/// `create_dir_all` returns `Ok` for a directory that already exists, whatever
/// its permissions — so on a machine where the privileged log dir exists but is
/// root-owned (`/var/log/nullgate`), an unprivileged daemon used to *select* it
/// and then panic inside `tracing_appender` ("initializing rolling file appender
/// failed: PermissionDenied") instead of falling back. Probing with a real file
/// is the only honest test; a metadata/permissions check wouldn't account for
/// ACLs, SELinux, or a read-only mount.
fn is_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".nullgate-write-probe-{}", std::process::id()));
    match OpenOptions::new().create(true).append(true).open(&probe) {
        Ok(_) => {
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Initialise tracing (rolling daily file + stderr) and install the crash hook.
///
/// Returns the `WorkerGuard` (which must be held for the lifetime of the process
/// so the non-blocking file writer is flushed on a clean exit) and the resolved
/// log directory (for logging where we ended up). Safe to call once at startup.
pub fn init(data_dir: &Path) -> (WorkerGuard, PathBuf) {
    let log_dir = resolve_log_dir(data_dir);
    let crash_path = log_dir.join(CRASH_LOG);

    let file_appender = tracing_appender::rolling::daily(&log_dir, format!("{LOG_STEM}.log"));
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);

    let env_filter = || {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,iroh=warn"))
    };

    // File layer (no ANSI) always on. The stderr layer only makes sense with a
    // real console (foreground dev run) — under a service there's no console, and
    // we instead reclaim the raw stderr handle to capture the Rust runtime's own
    // fatal messages (stack overflow, alloc failure, "fatal runtime error") that
    // never reach the panic hook. `Option<Layer>` implements `Layer`.
    let has_console = has_console();
    let file_layer = fmt::layer()
        .with_ansi(false)
        .with_writer(file_writer)
        .with_filter(env_filter());
    let stderr_layer = has_console.then(|| fmt::layer().with_filter(env_filter()));

    let _ = tracing_subscriber::registry()
        .with(file_layer)
        .with(stderr_layer)
        .try_init();

    install_panic_hook(crash_path.clone());

    // Under a service (no console), catch the abort classes the panic hook can't:
    // redirect the raw stderr handle to the crash log and install a vectored
    // exception handler for hardware faults (AV / stack overflow).
    #[cfg(windows)]
    if !has_console {
        crash_win::install(&crash_path);
    }

    (guard, log_dir)
}

/// Whether the process has an attached console. On Windows a service has none,
/// which is how we decide to reclaim stderr for crash capture. Elsewhere we keep
/// the stderr layer (systemd/launchd capture it).
fn has_console() -> bool {
    // Test hook: pretend we're a service (no console) so the crash-capture path
    // can be exercised from a terminal.
    if std::env::var_os("NULLGATE_FORCE_NO_CONSOLE").is_some() {
        return false;
    }
    #[cfg(windows)]
    unsafe {
        !windows_sys::Win32::System::Console::GetConsoleWindow().is_null()
    }
    #[cfg(not(windows))]
    {
        true
    }
}

/// Chain a panic hook that records the panic to `tracing` **and** appends it
/// synchronously to the crash log, so the reason survives even when the panic
/// escalates to an immediate `abort()` (the observed `0xc0000409` fastfail).
fn install_panic_hook(crash_log: PathBuf) {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // `panic::Location` and the payload message do not depend on debuginfo,
        // so they survive `strip = true` — this is the golden "src/foo.rs:123"
        // that pins the culprit even in a release build.
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "<unknown>".to_string());
        let msg = payload_str(info);
        let backtrace = std::backtrace::Backtrace::force_capture();

        tracing::error!(target: "panic", "daemon panicked at {location}: {msg}");

        // Synchronous, flushed append — must not rely on the async log writer.
        let record = format!(
            "\n==== PANIC {stamp} (pid {pid}) ====\n\
             location: {location}\n\
             message : {msg}\n\
             version : {ver}\n\
             backtrace:\n{backtrace}\n",
            stamp = now_rfc3339_ish(),
            pid = std::process::id(),
            ver = env!("CARGO_PKG_VERSION"),
        );
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&crash_log) {
            let _ = f.write_all(record.as_bytes());
            let _ = f.flush();
        }

        // Preserve default behaviour (stderr print + any abort semantics).
        default_hook(info);
    }));
}

/// Append a synchronous, flushed note to the crash log — the same durable sink
/// the panic hook uses. For events we want on disk even though the async tracing
/// writer may not flush before the process exits: e.g. the memory watchdog
/// recording *why* it forced a restart immediately before calling `exit`.
pub fn append_crash_note(data_dir: &Path, header: &str, body: &str) {
    let path = resolve_log_dir(data_dir).join(CRASH_LOG);
    let record = format!(
        "\n==== {header} {stamp} (pid {pid}) ====\n{body}\n",
        stamp = now_rfc3339_ish(),
        pid = std::process::id(),
    );
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(record.as_bytes());
        let _ = f.flush();
    }
}

fn payload_str(info: &std::panic::PanicHookInfo<'_>) -> String {
    let p = info.payload();
    if let Some(s) = p.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

/// A coarse timestamp without pulling in a datetime crate: seconds since the
/// Unix epoch, which is enough to correlate a crash-log entry with the Windows
/// event log / rolling daily file.
fn now_rfc3339_ish() -> String {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => format!("epoch+{}s", d.as_secs()),
        Err(_) => "epoch-unknown".to_string(),
    }
}

/// Windows-only capture for the crash classes the panic hook misses. A
/// `0xc0000409` fastfail (what our recurring crash actually is) comes from
/// `abort()` — a stack overflow, an allocation failure, or a native `abort()` —
/// and deliberately bypasses SEH, vectored handlers, and the unhandled-exception
/// filter, so no in-process handler can intercept it. Two nets that *do* work:
///
/// 1. **Reclaim raw stderr.** Before it aborts, the Rust runtime prints the
///    reason ("thread '…' has overflowed its stack", "memory allocation of N
///    bytes failed", "fatal runtime error: …") to fd 2, which a service discards.
///    Pointing `STD_ERROR_HANDLE` at the crash log captures it.
/// 2. **Vectored exception handler.** Hardware faults (access violation, stack
///    overflow) dispatch through a VEH *first-chance*, before they convert to an
///    abort, so we can log the code + faulting address. (The fastfail itself
///    won't reach here — that's what WER LocalDumps is for.)
#[cfg(windows)]
mod crash_win {
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;
    use std::sync::atomic::{AtomicIsize, Ordering};

    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::WriteFile;
    use windows_sys::Win32::System::Console::{SetStdHandle, STD_ERROR_HANDLE};
    use windows_sys::Win32::System::Diagnostics::Debug::{
        AddVectoredExceptionHandler, EXCEPTION_POINTERS,
    };

    /// Raw handle to the crash log, kept for the process lifetime. Backs both the
    /// stderr redirect and the exception handler's async-signal-safe writes.
    static CRASH_HANDLE: AtomicIsize = AtomicIsize::new(0);

    pub fn install(crash_log: &Path) {
        if let Ok(f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(crash_log)
        {
            let h = f.as_raw_handle() as HANDLE;
            // Route the Rust runtime's fatal stderr messages into the crash log.
            unsafe { SetStdHandle(STD_ERROR_HANDLE, h) };
            CRASH_HANDLE.store(h as isize, Ordering::SeqCst);
            // Keep the handle open for the whole process; do not close it.
            std::mem::forget(f);
        }
        // First = 1: run our handler before others. It only *reads* the record
        // and always continues the search, so it never changes crash behaviour.
        unsafe { AddVectoredExceptionHandler(1, Some(veh)) };
    }

    /// Append raw bytes to the crash log with no heap/lock — safe to call from an
    /// exception handler on an already-damaged (e.g. overflowed) stack.
    fn write_raw(bytes: &[u8]) {
        let h = CRASH_HANDLE.load(Ordering::SeqCst) as HANDLE;
        if h.is_null() {
            return;
        }
        let mut written = 0u32;
        unsafe {
            WriteFile(
                h,
                bytes.as_ptr(),
                bytes.len() as u32,
                &mut written,
                std::ptr::null_mut(),
            )
        };
    }

    const EXCEPTION_CONTINUE_SEARCH: i32 = 0;

    unsafe extern "system" fn veh(info: *mut EXCEPTION_POINTERS) -> i32 {
        if info.is_null() {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        let rec = (*info).ExceptionRecord;
        if rec.is_null() {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        let code = (*rec).ExceptionCode as u32;
        // Only the fatal codes; everything else (breakpoints, C++/SEH bookkeeping
        // exceptions) returns immediately so we add no noise or overhead.
        let name: &[u8] = match code {
            0xC0000005 => b"ACCESS_VIOLATION",
            0xC00000FD => b"STACK_OVERFLOW",
            0xC000001D => b"ILLEGAL_INSTRUCTION",
            0xC0000409 => b"STACK_BUFFER_OVERRUN",
            _ => return EXCEPTION_CONTINUE_SEARCH,
        };
        let addr = (*rec).ExceptionAddress as usize;

        // Format into a fixed stack buffer — no allocation.
        let mut buf = [0u8; 128];
        let mut n = 0usize;
        let mut put = |src: &[u8]| {
            for &b in src {
                if n < buf.len() {
                    buf[n] = b;
                    n += 1;
                }
            }
        };
        put(b"\n==== HARDWARE EXCEPTION (pre-abort) ====\ncode: 0x");
        let mut hex = [0u8; 16];
        put(hex_into(&mut hex, code as u64, 8));
        put(b" (");
        put(name);
        put(b")\naddr: 0x");
        put(hex_into(&mut hex, addr as u64, 16));
        put(b"\n");
        write_raw(&buf[..n]);

        // Let Rust's own handler / WER proceed exactly as before.
        EXCEPTION_CONTINUE_SEARCH
    }

    /// Write `digits` hex chars of `val` into `out`, returning the filled slice.
    fn hex_into(out: &mut [u8; 16], val: u64, digits: usize) -> &[u8] {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for (i, slot) in out.iter_mut().take(digits).enumerate() {
            let shift = (digits - 1 - i) * 4;
            *slot = HEX[((val >> shift) & 0xf) as usize];
        }
        &out[..digits]
    }
}
