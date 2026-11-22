//! Windows timer resolution for the 20 ms mixer tick.
//!
//! Windows wakes sleeping threads on its timer interrupt, which by default fires
//! every 15.6 ms. A 20 ms tokio interval therefore wakes after two interrupts,
//! about 31 ms: measured 2026-11-22 on the live host (Ryzen 7 6800H, Windows 11),
//! 321 ticks in 10 s instead of 500. The mixer then produced ~32 frames a second
//! for viewers that play 50, so they ran dry and stalled, while speakers' frames
//! piled up in the jitter buffers. With 1 ms resolution requested, the same probe
//! gave 500 ticks, every gap 20-22 ms. (wolfvoice, which this derives from, runs
//! mostly on Linux, where timers are already fine.)
//!
//! Since Windows 10 2004 the resolution is per process, so this process must ask
//! for it itself; another program asking does not help. Windows 11 may also stop
//! honouring the request for a process it decides is in the background unless
//! that process opts out of timer-resolution throttling, so we do that too. Both
//! settings end with the process; nothing to undo.

#[link(name = "winmm")]
extern "system" {
    fn timeBeginPeriod(period_ms: u32) -> u32;
}

#[repr(C)]
struct ProcessPowerThrottlingState {
    version: u32,
    control_mask: u32,
    state_mask: u32,
}

const PROCESS_POWER_THROTTLING: i32 = 4; // PROCESS_INFORMATION_CLASS::ProcessPowerThrottling
const PROCESS_POWER_THROTTLING_CURRENT_VERSION: u32 = 1;
const PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION: u32 = 0x4;

#[link(name = "kernel32")]
extern "system" {
    fn GetCurrentProcess() -> isize;
    fn SetProcessInformation(process: isize, class: i32, info: *const core::ffi::c_void, size: u32) -> i32;
}

/// Ask for 1 ms timer resolution for this process, and for it to be kept when
/// Windows considers the process idle or in the background. Logs the outcome.
pub fn request_fine_timer() {
    // Control bit set, state bit clear: "always honour this process's timer
    // resolution request". Older Windows 10 builds reject the bit; harmless.
    let state = ProcessPowerThrottlingState {
        version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        control_mask: PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
        state_mask: 0,
    };
    // SAFETY: a pseudo-handle for this process and a correctly sized, live struct.
    let kept = unsafe {
        SetProcessInformation(
            GetCurrentProcess(),
            PROCESS_POWER_THROTTLING,
            &state as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<ProcessPowerThrottlingState>() as u32,
        )
    } != 0;

    // SAFETY: plain Win32 call with an integer argument.
    let result = unsafe { timeBeginPeriod(1) };
    if result == 0 {
        log::info!(
            "Windows timer resolution set to 1 ms for the 20 ms mixer tick{}",
            if kept { "" } else { " (could not opt out of background throttling; older Windows?)" }
        );
    } else {
        log::warn!(
            "could not set Windows timer resolution to 1 ms (timeBeginPeriod returned {result}); \
             the mixer will tick about every 31 ms instead of 20 and voice will stall"
        );
    }
}
