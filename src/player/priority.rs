//! Elevate the calling thread's priority / QoS so OS-level throttling
//! (background demotion, eco-cores, powersave clocks) doesn't slip
//! frame deadlines. Best-effort: if the syscall fails (sandboxed
//! environment, missing CAP_SYS_NICE, etc.) we log and continue at
//! default priority. No panic, no functional regression.
//!
//! Call ONCE, from the player thread itself (not the spawner) so the
//! per-thread settings apply to the right thread.

pub fn elevate_player_thread() {
    #[cfg(target_os = "macos")]
    macos::raise();
    #[cfg(target_os = "linux")]
    linux::raise();
    #[cfg(target_os = "windows")]
    windows::raise();
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    eprintln!("[player] priority: unsupported platform — default scheduling");
}

#[cfg(target_os = "macos")]
mod macos {
    // QOS_CLASS_USER_INTERACTIVE = 0x21 — the highest non-realtime class.
    // Default for spawned threads on macOS is UTILITY (0x11), which the
    // OS happily demotes when the app loses focus and is willing to park
    // on Efficiency cores. USER_INTERACTIVE stays on Performance cores
    // and isn't demoted on focus loss.
    extern "C" {
        fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
    }
    pub fn raise() {
        let rc = unsafe { pthread_set_qos_class_self_np(0x21, 0) };
        eprintln!("[player] priority: macOS QOS_CLASS_USER_INTERACTIVE (rc={rc})");
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use libc::{c_int, sched_param, setpriority, PRIO_PROCESS, SCHED_FIFO};

    extern "C" {
        fn sched_setscheduler(pid: c_int, policy: c_int, param: *const sched_param) -> c_int;
    }

    pub fn raise() {
        // SCHED_FIFO requires CAP_SYS_NICE — works for users in the
        // `audio` / `realtime` group on most distros, fails otherwise.
        let p = sched_param { sched_priority: 10 };
        let fifo = unsafe { sched_setscheduler(0, SCHED_FIFO, &p) };
        if fifo == 0 {
            eprintln!("[player] priority: Linux SCHED_FIFO prio=10");
            return;
        }
        // Fall back to nice value — always allowed, more modest effect.
        // 0 = current thread; -10 is well below default 0 but above the
        // -20 "system" range that needs CAP_SYS_NICE.
        let nice = unsafe { setpriority(PRIO_PROCESS, 0, -10) };
        eprintln!(
            "[player] priority: Linux SCHED_FIFO unavailable (likely EPERM); \
             fell back to nice -10 (rc={nice})"
        );
    }
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
