//! macOS sleep/wake backend, built on powerd's `IOPMConnection`.
//!
//! The documented API — `IORegisterForSystemPower` — reports a dark wake and a real
//! wake identically (`kIOMessageSystemHasPoweredOn` for both), and that distinction
//! is the entire point of this module. `IOPMConnection` instead hands the callback
//! the system's *capability* bits, so a wake that brings up the CPU and the network
//! but not graphics is recognisable as a dark wake.
//!
//! Its symbols are exported from IOKit and have been stable since 10.6, but Apple
//! ships no public header for them, so the declarations below are transcribed from
//! `IOPMLibPrivate.h` in Apple's open-source PowerManagement project. The capability
//! bits we actually read are cross-checked against the **public**
//! `IOKit/pwr_mgt/IOPM.h`, whose `kIOPMSystemCapability{CPU,Graphics,Audio,Network}`
//! occupy the same low bits — deliberately, we never touch the higher bits, where
//! the two enums disagree (the private one has Disk where the public one has AOT).
//!
//! If the connection can't be created we log and give up. The daemon then behaves
//! exactly as it did before this module existed — awake across sleep, chatty on dark
//! wake — rather than refusing to start over a power-management nicety.

use std::ffi::c_void;
use std::ptr;
use std::sync::Arc;
use std::time::Duration;

use core_foundation_sys::base::{CFRelease, CFTypeRef};
use core_foundation_sys::runloop::{kCFRunLoopDefaultMode, CFRunLoopGetCurrent, CFRunLoopRef, CFRunLoopRun};
use core_foundation_sys::string::{kCFStringEncodingUTF8, CFStringCreateWithCString, CFStringRef};

use super::PowerHandler;

/// How long the disconnect may take before we stop holding sleep off. powerd allows
/// roughly 30s before it force-sleeps us anyway; closing a handful of QUIC
/// connections needs milliseconds, so this is a backstop, not a budget.
const SLEEP_DISCONNECT_TIMEOUT: Duration = Duration::from_secs(5);

// Capability bits, from the public `IOKit/pwr_mgt/IOPM.h`.
const CAP_CPU: u32 = 0x01; // kIOPMSystemCapabilityCPU
const CAP_GRAPHICS: u32 = 0x02; // kIOPMSystemCapabilityGraphics (kIOPMCapabilityVideo)
const CAP_AUDIO: u32 = 0x04; // kIOPMSystemCapabilityAudio
const CAP_NETWORK: u32 = 0x08; // kIOPMSystemCapabilityNetwork

/// Capability changes we ask to be notified about. CPU is implicit — it changes on
/// every sleep and wake regardless. Graphics is what separates a full wake from a
/// dark wake; network and audio come along because a state change that touches them
/// is one we want to see.
const INTERESTS: u32 = CAP_GRAPHICS | CAP_AUDIO | CAP_NETWORK;

const KERN_SUCCESS: i32 = 0;

type IOReturn = i32;
type IOPMConnection = *mut c_void;
type IOPMConnectionMessageToken = u32;
type IOPMSystemPowerStateCapabilities = u32;

type IOPMEventHandlerType = extern "C" fn(
    param: *mut c_void,
    connection: IOPMConnection,
    token: IOPMConnectionMessageToken,
    capabilities: IOPMSystemPowerStateCapabilities,
);

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOPMConnectionCreate(
        my_name: CFStringRef,
        interests: u32,
        new_connection: *mut IOPMConnection,
    ) -> IOReturn;
    fn IOPMConnectionSetNotification(
        my_connection: IOPMConnection,
        param: *mut c_void,
        handler: IOPMEventHandlerType,
    ) -> IOReturn;
    fn IOPMConnectionScheduleWithRunLoop(
        my_connection: IOPMConnection,
        run_loop: CFRunLoopRef,
        run_loop_mode: CFStringRef,
    ) -> IOReturn;
    fn IOPMConnectionAcknowledgeEvent(
        connection: IOPMConnection,
        token: IOPMConnectionMessageToken,
    ) -> IOReturn;
}

/// What a capability change means for us.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transition {
    /// The machine is going to sleep (the CPU is leaving).
    Sleep,
    /// A maintenance wake: CPU and network, but no graphics. The user isn't there
    /// and the machine will drop back to sleep in seconds.
    DarkWake,
    /// A real wake, with graphics.
    FullWake,
}

/// Mirrors the `IOPMIsASleep` / `IOPMIsADarkWake` / `IOPMIsAUserWake` macros.
fn classify(capabilities: u32) -> Transition {
    if capabilities & CAP_CPU == 0 {
        Transition::Sleep
    } else if capabilities & CAP_GRAPHICS == 0 {
        Transition::DarkWake
    } else {
        Transition::FullWake
    }
}

/// Handed to powerd as the callback's `param`. Leaked for the life of the process,
/// because the run loop it serves never returns.
struct Ctx {
    handler: Arc<PowerHandler>,
    runtime: tokio::runtime::Handle,
}

/// Run a CFRunLoop on its own thread and let powerd drive it. Must be called from
/// within the Tokio runtime, whose handle the callback borrows to reach the engine.
pub(crate) fn spawn(handler: Arc<PowerHandler>) {
    let ctx = Box::new(Ctx {
        handler,
        runtime: tokio::runtime::Handle::current(),
    });
    if let Err(e) = std::thread::Builder::new()
        .name("nullgate-power".into())
        .spawn(move || run(ctx))
    {
        tracing::warn!("power: could not start the sleep/wake thread: {e}");
    }
}

fn run(ctx: Box<Ctx>) {
    // SAFETY: every pointer below is either freshly created by CoreFoundation (and
    // released once), a valid stack slot, or `ctx` leaked deliberately to outlive
    // the run loop. `CFRunLoopRun` never returns, so nothing here is reachable twice.
    unsafe {
        let name = CFStringCreateWithCString(ptr::null(), c"Nullgate".as_ptr(), kCFStringEncodingUTF8);
        let mut conn: IOPMConnection = ptr::null_mut();
        let rc = IOPMConnectionCreate(name, INTERESTS, &mut conn);
        CFRelease(name as CFTypeRef);
        if rc != KERN_SUCCESS || conn.is_null() {
            tracing::warn!("power: IOPMConnectionCreate failed (0x{rc:x}); sleep/wake unhandled");
            return;
        }

        let ctx = Box::into_raw(ctx);
        let rc = IOPMConnectionSetNotification(conn, ctx.cast(), on_power_event);
        if rc != KERN_SUCCESS {
            tracing::warn!("power: IOPMConnectionSetNotification failed (0x{rc:x})");
            drop(Box::from_raw(ctx));
            return;
        }
        let rc = IOPMConnectionScheduleWithRunLoop(conn, CFRunLoopGetCurrent(), kCFRunLoopDefaultMode);
        if rc != KERN_SUCCESS {
            tracing::warn!("power: IOPMConnectionScheduleWithRunLoop failed (0x{rc:x})");
            drop(Box::from_raw(ctx));
            return;
        }

        tracing::info!("power: watching for system sleep/wake");
        CFRunLoopRun();
    }
}

extern "C" fn on_power_event(
    param: *mut c_void,
    connection: IOPMConnection,
    token: IOPMConnectionMessageToken,
    capabilities: IOPMSystemPowerStateCapabilities,
) {
    // SAFETY: `param` is the `Ctx` leaked in `run`, which outlives every callback.
    let ctx = unsafe { &*(param as *const Ctx) };

    match classify(capabilities) {
        Transition::Sleep => {
            // This must finish before we acknowledge: powerd stops waiting for us the
            // moment it has the ack, and the machine freezes wherever we happen to be.
            let handler = ctx.handler.clone();
            let done = ctx.runtime.block_on(async move {
                tokio::time::timeout(SLEEP_DISCONNECT_TIMEOUT, handler.on_sleep()).await
            });
            if done.is_err() {
                tracing::warn!("power: disconnect did not finish before sleep; acknowledging anyway");
            }
        }
        // Nothing to do, and that is the whole fix: staying offline through a
        // maintenance wake is what stops the peers' "came online" notifications.
        Transition::DarkWake => tracing::debug!("power: dark wake — staying offline"),
        Transition::FullWake => {
            let handler = ctx.handler.clone();
            ctx.runtime.spawn(async move { handler.on_wake().await });
        }
    }

    // SAFETY: `connection` and `token` are powerd's, valid for this callback.
    unsafe {
        IOPMConnectionAcknowledgeEvent(connection, token);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sleep is the absence of the CPU bit — capabilities go to zero.
    #[test]
    fn no_cpu_is_sleep() {
        assert_eq!(classify(0), Transition::Sleep);
    }

    /// The case this module exists for: Power Nap's maintenance wake brings up the
    /// CPU and the network, but never graphics.
    #[test]
    fn cpu_and_network_without_graphics_is_dark_wake() {
        assert_eq!(classify(CAP_CPU | CAP_NETWORK), Transition::DarkWake);
    }

    /// A user opening the lid gets graphics.
    #[test]
    fn graphics_is_a_full_wake() {
        assert_eq!(
            classify(CAP_CPU | CAP_GRAPHICS | CAP_AUDIO | CAP_NETWORK),
            Transition::FullWake
        );
    }

    /// Graphics without a CPU is nonsense; the CPU bit decides first, as it does in
    /// `IOPMIsASleep`. Guards against reordering the branches.
    #[test]
    fn cpu_bit_dominates_graphics() {
        assert_eq!(classify(CAP_GRAPHICS), Transition::Sleep);
    }
}
