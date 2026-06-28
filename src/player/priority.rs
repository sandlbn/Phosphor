//! Elevate the calling thread's priority so OS-level throttling doesn't
//! slip frame deadlines. Windows-only for now.
//!
//! Call ONCE, from the player thread itself (not the spawner) so the
//! per-thread settings apply to the right thread.

pub fn elevate_player_thread() {
    #[cfg(target_os = "windows")]
    windows::raise();
}

#[cfg(target_os = "windows")]
mod windows {
    // winapi via extern "system" — same pattern as the timeBeginPeriod
    // call in `windows_timer` in src/main.rs.
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentThread() -> *mut core::ffi::c_void;
        fn GetCurrentProcess() -> *mut core::ffi::c_void;
        fn SetThreadPriority(thread: *mut core::ffi::c_void, priority: i32) -> i32;
        fn SetPriorityClass(process: *mut core::ffi::c_void, class: u32) -> i32;
    }
    const THREAD_PRIORITY_TIME_CRITICAL: i32 = 15;
    const ABOVE_NORMAL_PRIORITY_CLASS: u32 = 0x0000_8000;

    pub fn raise() {
        unsafe {
            let t = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
            let p = SetPriorityClass(GetCurrentProcess(), ABOVE_NORMAL_PRIORITY_CLASS);
            eprintln!(
                "[player] priority: Windows TIME_CRITICAL + ABOVE_NORMAL \
                 (thread={t}, proc={p})"
            );
        }
    }
}
