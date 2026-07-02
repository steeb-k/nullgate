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
/// falling back to `<data_dir>/logs`, then the temp dir. Always returns a dir
/// that was successfully created.
fn resolve_log_dir(data_dir: &Path) -> PathBuf {
    let candidates = [
        preferred_log_dir(),
        data_dir.join("logs"),
        std::env::temp_dir().join("nullgate").join("logs"),
    ];
    for dir in candidates.iter() {
        if fs::create_dir_all(dir).is_ok() {
            return dir.clone();
        }
    }
    // Last resort: the temp dir itself (create_dir_all of it is a no-op).
    std::env::temp_dir()
}

/// Initialise tracing (rolling daily file + stderr) and install the crash hook.
///
/// Returns the `WorkerGuard` (which must be held for the lifetime of the process
/// so the non-blocking file writer is flushed on a clean exit) and the resolved
/// log directory (for logging where we ended up). Safe to call once at startup.
pub fn init(data_dir: &Path) -> (WorkerGuard, PathBuf) {
    let log_dir = resolve_log_dir(data_dir);

    let file_appender = tracing_appender::rolling::daily(&log_dir, format!("{LOG_STEM}.log"));
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);

    let env_filter = || {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,iroh=warn"))
    };

    // File layer (no ANSI); stderr layer keeps foreground / systemd / launchd
    // output working as before. Each gets its own filter instance.
    let file_layer = fmt::layer()
        .with_ansi(false)
        .with_writer(file_writer)
        .with_filter(env_filter());
    let stderr_layer = fmt::layer().with_filter(env_filter());

    let _ = tracing_subscriber::registry()
        .with(file_layer)
        .with(stderr_layer)
        .try_init();

    install_panic_hook(log_dir.join(CRASH_LOG));
    (guard, log_dir)
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
