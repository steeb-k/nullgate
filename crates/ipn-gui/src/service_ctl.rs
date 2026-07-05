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

use std::process::Command;

/// (Re)start the privileged Nullgate service via the OS graphical elevation
/// prompt. Blocks until the prompt is dismissed. `Err` carries a user-facing
/// message (cancelled prompt, missing elevation tool, or a failed start).
pub fn restart_daemon_service() -> Result<(), String> {
    #[cfg(windows)]
    {
        // Outer (unprivileged) PowerShell re-launches an elevated PowerShell via
        // `Start-Process -Verb RunAs` (the UAC prompt). `-PassThru`/`exit` propagate
        // the inner exit code; the try/catch turns a declined UAC into exit 1.
        let inner = "try { $p = Start-Process -Verb RunAs -Wait -PassThru -ErrorAction Stop \
             -FilePath 'powershell' -ArgumentList '-NoProfile','-WindowStyle','Hidden','-Command',\
             'Stop-Service NullgateDaemon -ErrorAction SilentlyContinue; Start-Service NullgateDaemon'; \
             exit $p.ExitCode } catch { exit 1 }";
        run(
            Command::new("powershell").args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", inner]),
            "Elevation was cancelled, or the service couldn't start.",
        )
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
#[cfg(any(windows, target_os = "macos"))]
fn run(cmd: &mut Command, friendly: &str) -> Result<(), String> {
    match cmd.output() {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => Err(status_error(&out, friendly)),
        Err(e) => Err(format!("Couldn't launch the elevation prompt: {e}")),
    }
}

/// Fold a trimmed stderr snippet into the user-facing message for diagnostics.
#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
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
