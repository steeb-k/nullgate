//! Memory watchdog — a stopgap for iroh's unbounded per-remote mapped-address
//! cache (`socket::mapped_addrs::AddrMap`, upstream n0-computer/iroh#4293).
//!
//! That cache is never pruned, so under address churn the daemon's resident
//! memory climbs until a hashmap reallocation asks for tens of GiB and Rust
//! aborts (the observed `0xc0000409` OOM — an 80 GiB request in the captured
//! minidump). The maps live inside the iroh node, which is built once in
//! `Engine::start` and never rebuilt, so `set_online(false/true)` does *not* free
//! them — only tearing down the process does. This samples our own RSS and, past
//! a limit, logs why and exits with a non-zero code so the service manager (SCM /
//! systemd / launchd, already configured for auto-restart) brings us back with the
//! maps cleared — bounding memory far below the abort. Remove once #4293 ships.
//!
//! The restart causes a brief presence blip on the mesh; the GUI debounces the
//! resulting "came online" notifications (see `ipn-gui`'s `notify_newly_online`).

use std::path::PathBuf;
use std::time::Duration;

/// Exit code the watchdog uses to force a restart. Non-zero so Windows SCM (with
/// `set_failure_actions_on_non_crash_failures`) and systemd `Restart=on-failure`
/// treat it as a restartable failure rather than an intentional stop.
pub const MEM_RESTART_EXIT_CODE: i32 = 92;

/// Default RSS ceiling. Normal daemon footprint is tens–low-hundreds of MB, so
/// this leaves ample headroom while still tripping long before the multi-GB
/// runaway that precedes the abort.
const DEFAULT_LIMIT_MB: u64 = 1024;
const DEFAULT_INTERVAL_SECS: u64 = 30;

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

/// Spawn the watchdog onto the current Tokio runtime. `data_dir` is used only to
/// locate the crash log for a synchronous "why we restarted" note (the async
/// tracing writer can't be relied on to flush before `exit`). No-op if the limit
/// is `NULLGATE_MEM_LIMIT_MB=0` or RSS can't be sampled on this platform.
pub fn spawn(data_dir: PathBuf) {
    let limit_mb = env_u64("NULLGATE_MEM_LIMIT_MB", DEFAULT_LIMIT_MB);
    if limit_mb == 0 {
        tracing::info!("memory watchdog disabled (NULLGATE_MEM_LIMIT_MB=0)");
        return;
    }
    if current_rss_bytes().is_none() {
        tracing::warn!("memory watchdog: RSS sampling unsupported on this platform; disabled");
        return;
    }
    let interval = Duration::from_secs(env_u64("NULLGATE_MEM_CHECK_SECS", DEFAULT_INTERVAL_SECS).max(1));
    let limit_bytes = limit_mb.saturating_mul(1024 * 1024);
    tracing::info!(
        "memory watchdog armed: limit {limit_mb} MB, checking every {}s (iroh#4293 stopgap)",
        interval.as_secs()
    );

    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let Some(rss) = current_rss_bytes() else { continue };
            if rss < limit_bytes {
                continue;
            }
            let rss_mb = rss / (1024 * 1024);
            let body = format!(
                "resident memory {rss_mb} MB reached the {limit_mb} MB watchdog limit; \
                 forcing a restart to reclaim iroh's unbounded mapped-address cache \
                 (n0-computer/iroh#4293). The service manager will restart the daemon."
            );
            tracing::error!(target: "watchdog", "{body}");
            // Durable note in case the async log writer can't flush before exit.
            crate::logging::append_crash_note(&data_dir, "MEMORY WATCHDOG RESTART", &body);
            // Bypass the clean service-stop path so SCM / systemd / launchd see a
            // failure and restart us; a clean exit(0) would just stop the service.
            std::process::exit(MEM_RESTART_EXIT_CODE);
        }
    });
}

/// Current resident set size in bytes, or `None` if unsupported on this platform.
fn current_rss_bytes() -> Option<u64> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::ProcessStatus::{
            K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        // SAFETY: zeroed POD struct; we pass its size and check the BOOL result.
        unsafe {
            let mut counters: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
            let cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
            if K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, cb) != 0 {
                Some(counters.WorkingSetSize as u64)
            } else {
                None
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        // /proc/self/status VmRSS is current (not peak) resident memory, in kB.
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
                return Some(kb * 1024);
            }
        }
        None
    }
    #[cfg(target_os = "macos")]
    {
        // No /proc on macOS; getrusage's ru_maxrss is *peak* RSS in bytes here.
        // Peak tracks current closely for a monotonic leak, which is all we guard.
        // SAFETY: zeroed POD struct filled by the kernel; result checked.
        unsafe {
            let mut usage: libc::rusage = std::mem::zeroed();
            if libc::getrusage(libc::RUSAGE_SELF, &mut usage) == 0 {
                Some(usage.ru_maxrss as u64)
            } else {
                None
            }
        }
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        None
    }
}
