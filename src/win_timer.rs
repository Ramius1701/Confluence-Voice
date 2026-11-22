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
//! Later runs showed the 1 ms request alone is not dependable (see
//! spawn_tick_thread), so the mixer is now driven by a high-resolution waitable
//! timer on its own thread. The request is kept for the rest of the process's
//! timers (tokio sleeps, WebRTC's own timers).
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
    fn CreateWaitableTimerExW(attr: *const core::ffi::c_void, name: *const u16, flags: u32, access: u32) -> isize;
    fn SetWaitableTimer(
        timer: isize,
        due: *const i64,
        period_ms: i32,
        completion: *const core::ffi::c_void,
        arg: *const core::ffi::c_void,
        resume: i32,
    ) -> i32;
    fn WaitForSingleObject(handle: isize, ms: u32) -> u32;
    fn CloseHandle(handle: isize) -> i32;
    fn GetCurrentThread() -> isize;
    fn SetThreadPriority(thread: isize, priority: i32) -> i32;
}

const CREATE_WAITABLE_TIMER_HIGH_RESOLUTION: u32 = 0x2;
const TIMER_ALL_ACCESS: u32 = 0x1F_0003;
const INFINITE: u32 = 0xFFFF_FFFF;
const THREAD_PRIORITY_HIGHEST: i32 = 2;

/// Start a thread that ticks every `period` on a high-resolution waitable timer
/// (Windows 10 1803+), and return the receiving end. None if that timer kind is
/// not available, in which case the caller falls back to tokio's interval.
///
/// Why: timeBeginPeriod alone proved unreliable. On the live host it gave a clean
/// 20 ms interval in one run, and 31 ms again in a later run with the request in
/// place, while the high-resolution waitable timer gave 500/500 ticks, every gap
/// at most 20.7 ms, in every run, with or without it. Deadlines are absolute
/// (start + n * period), so the tick never drifts.
///
/// The channel holds one tick. If the mixer is still busy when the next one is
/// due, that tick is dropped (it is already owed one), as MissedTickBehavior::Delay
/// did. After a long gap (the PC slept, say) the schedule restarts from now instead
/// of firing a burst of overdue ticks.
pub fn spawn_tick_thread(period: std::time::Duration) -> Option<tokio::sync::mpsc::Receiver<()>> {
    use std::time::Instant;

    // SAFETY: plain Win32 call; null attributes and name are allowed.
    let timer = unsafe {
        CreateWaitableTimerExW(
            std::ptr::null(),
            std::ptr::null(),
            CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
            TIMER_ALL_ACCESS,
        )
    };
    if timer == 0 {
        return None;
    }
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let spawned = std::thread::Builder::new().name("mixer-tick".into()).spawn(move || {
        // SAFETY: pseudo-handle for this thread. Failure only costs precision.
        unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) };
        let mut next = Instant::now();
        loop {
            next += period;
            let now = Instant::now();
            if next > now {
                // Negative due time = relative, in 100 ns units.
                let due = -(((next - now).as_nanos() / 100) as i64);
                // SAFETY: valid timer handle owned by this thread; no completion routine.
                unsafe {
                    SetWaitableTimer(timer, &due, 0, std::ptr::null(), std::ptr::null(), 0);
                    WaitForSingleObject(timer, INFINITE);
                }
            } else if now - next > period * 5 {
                next = now;
            }
            if let Err(tokio::sync::mpsc::error::TrySendError::Closed(())) = tx.try_send(()) {
                break;
            }
        }
        // SAFETY: closing the handle this thread created and no longer uses.
        unsafe { CloseHandle(timer) };
    });
    match spawned {
        Ok(_) => Some(rx),
        Err(e) => {
            log::warn!("could not start the mixer tick thread: {e}");
            // SAFETY: the thread never started, so the handle is still ours.
            unsafe { CloseHandle(timer) };
            None
        }
    }
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
            "Windows timer resolution set to 1 ms{}",
            if kept { "" } else { " (could not opt out of background throttling; older Windows?)" }
        );
    } else {
        log::warn!(
            "could not set Windows timer resolution to 1 ms (timeBeginPeriod returned {result}); \
             the mixer has its own high-resolution timer, but other timers stay coarse"
        );
    }
}
