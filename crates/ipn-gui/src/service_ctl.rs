//! Launching the privileged Nullgate service from the unprivileged GUI.
//!
//! Normally the daemon is installed once (elevated) and auto-starts forever, so
//! the GUI never needs privilege. But when the service is stopped or degraded the
//! banner offers a Start/Restart button — and *that* momentarily needs admin. We
//! don't hold elevation; instead we shell out to the OS's own graphical elevation
//! prompt (UAC / polkit / the macOS auth dialog) to run the platform service
//! manager's restart command. The call blocks until the prompt is dismissed, so it
//! must run off the GTK thread (see `Net::restart_service`). Restart-capable on
//! every platform, so one action covers both "stopped" and "running but degraded".

#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::process::Command;

/// (Re)start the privileged Nullgate service via the OS graphical elevation
/// prompt. Blocks until the prompt is dismissed. `Err` carries a user-facing
/// message (cancelled prompt, missing elevation tool, or a failed start).
pub fn restart_daemon_service() -> Result<(), String> {
    #[cfg(windows)]
    {
        // No PowerShell: elevate the (code-signed) daemon binary directly with the
        // Win32 "runas" verb and run its `restart` subcommand, waiting for the exit
        // code. The signed exe means the UAC dialog shows the Nullgate publisher.
        win::elevated_daemon_restart()
    }

    #[cfg(target_os = "linux")]
    {
        // pkexec pops the desktop's polkit password dialog, then runs systemctl as root.
        match Command::new("pkexec")
            .args(["systemctl", "restart", "nullgate-daemon.service"])
            .output()
        {
            Ok(out) if out.status.success() => Ok(()),
            Ok(out) => Err(status_error(
                &out,
                "The service couldn't be started (authorization cancelled or the unit is missing).",
            )),
            // pkexec absent → no graphical elevation available; point at the CLI path.
            Err(_) => Err("Couldn't find pkexec. Start it manually: sudo systemctl restart nullgate-daemon".into()),
        }
    }

    #[cfg(target_os = "macos")]
    {
        // `with administrator privileges` makes osascript raise the native auth dialog,
        // then kickstart -k restarts the (loaded) LaunchDaemon — or starts it if stopped.
        run(
            Command::new("osascript").args([
                "-e",
                "do shell script \"launchctl kickstart -k system/io.github.steeb_k.Nullgate.daemon\" \
                 with administrator privileges",
            ]),
            "Elevation was cancelled, or the service couldn't start.",
        )
    }

    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        Err("Starting the service isn't supported on this platform.".into())
    }
}

/// Run a command, mapping a non-zero exit / spawn failure to `friendly`.
#[cfg(target_os = "macos")]
fn run(cmd: &mut Command, friendly: &str) -> Result<(), String> {
    match cmd.output() {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => Err(status_error(&out, friendly)),
        Err(e) => Err(format!("Couldn't launch the elevation prompt: {e}")),
    }
}

/// Fold a trimmed stderr snippet into the user-facing message for diagnostics.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn status_error(out: &std::process::Output, friendly: &str) -> String {
    let err = String::from_utf8_lossy(&out.stderr);
    let err = err.trim();
    if err.is_empty() {
        friendly.to_string()
    } else {
        // Keep it short — the banner-toast has limited room.
        let snippet: String = err.lines().next().unwrap_or(err).chars().take(160).collect();
        format!("{friendly} ({snippet})")
    }
}

/// Windows: UAC-elevate our own `nullgate-daemon.exe restart` (no PowerShell, no
/// `sc.exe`) via `ShellExecuteExW`'s "runas" verb, then wait for its exit code.
#[cfg(windows)]
mod win {
    use std::os::windows::ffi::OsStrExt;
    use std::path::PathBuf;

    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_CANCELLED};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, WaitForSingleObject, INFINITE,
    };
    use windows_sys::Win32::UI::Shell::{
        ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

    /// The daemon exe sits next to this binary (both in `…/bin` when installed, or
    /// in `target/<profile>` in dev).
    fn daemon_exe() -> Result<PathBuf, String> {
        let exe = std::env::current_exe()
            .map_err(|e| format!("can't locate this program: {e}"))?;
        let dir = exe.parent().ok_or("this program has no parent directory")?;
        let daemon = dir.join("nullgate-daemon.exe");
        if daemon.exists() {
            Ok(daemon)
        } else {
            Err("Couldn't find nullgate-daemon.exe next to the app.".into())
        }
    }

    /// NUL-terminated UTF-16, for the `*const u16` Win32 string arguments.
    fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    pub fn elevated_daemon_restart() -> Result<(), String> {
        let daemon = daemon_exe()?;
        let file = wide(daemon.as_os_str());
        let verb = wide(std::ffi::OsStr::new("runas"));
        let params = wide(std::ffi::OsStr::new("restart"));

        // SAFETY: a zeroed SHELLEXECUTEINFOW with cbSize set is the documented way
        // to call ShellExecuteExW; all pointers outlive the call.
        let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOCLOSEPROCESS;
        info.lpVerb = verb.as_ptr();
        info.lpFile = file.as_ptr();
        info.lpParameters = params.as_ptr();
        info.nShow = SW_HIDE as i32;

        let ok = unsafe { ShellExecuteExW(&mut info) };
        if ok == 0 {
            // The most common failure is the user declining the UAC consent dialog.
            let err = unsafe { GetLastError() };
            if err == ERROR_CANCELLED {
                return Err("Elevation was cancelled.".into());
            }
            return Err(format!("Couldn't launch the elevated restart (error {err})."));
        }
        if info.hProcess.is_null() {
            // Elevation started but we got no handle to wait on; treat as best-effort.
            return Ok(());
        }
        // SAFETY: hProcess is a valid handle we own (SEE_MASK_NOCLOSEPROCESS); we
        // wait on it, read its exit code, and close it exactly once.
        let code = unsafe {
            WaitForSingleObject(info.hProcess, INFINITE);
            let mut code: u32 = 1;
            let got = GetExitCodeProcess(info.hProcess, &mut code);
            CloseHandle(info.hProcess);
            if got == 0 {
                0
            } else {
                code
            }
        };
        if code != 0 {
            Err("The service couldn't be restarted.".into())
        } else {
            Ok(())
        }
    }
}
