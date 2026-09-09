use std::ffi::{c_char, c_int, c_void};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const NOTIFY_STATUS_OK: u32 = 0;
const NOTIFY_KEY: &[u8] = b"com.apple.system.thermalpressurelevel\0";

const EVFILT_READ: i16 = -1;
const EVFILT_SIGNAL: i16 = -6;
const EVFILT_TIMER: i16 = -7;

const EV_ADD: u16 = 0x0001;
const EV_ENABLE: u16 = 0x0004;
const EV_DELETE: u16 = 0x0002;
const EV_ONESHOT: u16 = 0x0010;

const SIGINT: c_int = 2;
const SIGTERM: c_int = 15;

// Cooldown delay before restoring burst power (e.g., 120 seconds)
const COOLDOWN_MS: isize = 120_000;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct Kevent {
    ident: usize,
    filter: i16,
    flags: u16,
    fflags: u32,
    data: isize,
    udata: *mut c_void,
}

#[link(name = "System")]
unsafe extern "C" {
    fn notify_register_file_descriptor(
        name: *const c_char,
        notify_fd: *mut c_int,
        flags: c_int,
        out_token: *mut c_int,
    ) -> u32;

    fn notify_get_state(token: c_int, state64: *mut u64) -> u32;
    fn notify_cancel(token: c_int) -> u32;

    fn kqueue() -> c_int;
    fn kevent(
        kq: c_int,
        changelist: *const Kevent,
        nchanges: c_int,
        eventlist: *mut Kevent,
        nevents: c_int,
        timeout: *const c_void,
    ) -> c_int;

    fn signal(sig: c_int, handler: extern "C" fn(c_int)) -> usize;
    fn read(fd: c_int, buf: *mut u8, count: usize) -> isize;
    fn close(fd: c_int) -> c_int;
}

fn format_timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let sec = secs % 60;
    let min = (secs / 60) % 60;
    let hour = (secs / 3600) % 24;

    let days = (secs / 86400) as i64;
    let z = days + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let mut y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    if m <= 2 {
        y += 1;
    }

    format!("{y:04}-{m:02}-{d:02} {hour:02}:{min:02}:{sec:02} UTC")
}

macro_rules! log {
    ($($arg:tt)*) => {{
        eprintln!("[{}] {}", format_timestamp(), format_args!($($arg)*));
    }};
}

extern "C" fn handle_signal(_: c_int) {
    // No-op: kevent catches EVFILT_SIGNAL synchronously.
    // Registering a trivial handler prevents Darwin's default SIG_DFL termination.
}

fn set_low_power_mode(enable: bool) {
    let val = if enable { "1" } else { "0" };
    let status = Command::new("/usr/bin/pmset")
        .args(["-a", "lowpowermode", val])
        .status();

    match status {
        Ok(s) if s.success() => log!("Low Power Mode -> {val}"),
        Ok(s) => log!("pmset exited with status: {s}"),
        Err(e) => log!("Failed to invoke pmset: {e}"),
    }
}

fn describe_pressure_level(level: u64) -> &'static str {
    match level {
        0 => "Nominal (Full performance)",
        1 => "Moderate (Minor throttling / increased heat)",
        2 => "Heavy (Significant throttling)",
        3 => "Trapping (Emergency mitigation)",
        4 => "Sleeping (Forced sleep)",
        _ => "Unknown state",
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Intercept termination signals without executing default abort actions
    unsafe {
        signal(SIGTERM, handle_signal);
        signal(SIGINT, handle_signal);
    }

    let mut notify_fd: c_int = -1;
    let mut token: c_int = 0;

    let status = unsafe {
        notify_register_file_descriptor(
            NOTIFY_KEY.as_ptr() as *const c_char,
            &mut notify_fd,
            0,
            &mut token,
        )
    };

    if status != NOTIFY_STATUS_OK || notify_fd < 0 {
        log!("Failed to register notification listener: {status}");
        std::process::exit(1);
    }

    let kq = unsafe { kqueue() };
    if kq < 0 {
        log!("Failed to create kqueue descriptor");
        unsafe {
            close(notify_fd);
            notify_cancel(token);
        }
        std::process::exit(1);
    }

    // Subscribe to incoming pipe data and OS termination signals in a single kernel queue
    let change_list = [
        Kevent {
            ident: notify_fd as usize,
            filter: EVFILT_READ,
            flags: EV_ADD | EV_ENABLE,
            fflags: 0,
            data: 0,
            udata: std::ptr::null_mut(),
        },
        Kevent {
            ident: SIGTERM as usize,
            filter: EVFILT_SIGNAL,
            flags: EV_ADD | EV_ENABLE,
            fflags: 0,
            data: 0,
            udata: std::ptr::null_mut(),
        },
        Kevent {
            ident: SIGINT as usize,
            filter: EVFILT_SIGNAL,
            flags: EV_ADD | EV_ENABLE,
            fflags: 0,
            data: 0,
            udata: std::ptr::null_mut(),
        },
    ];

    let register_status = unsafe {
        kevent(
            kq,
            change_list.as_ptr(),
            change_list.len() as c_int,
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
        )
    };

    if register_status < 0 {
        log!("Failed to register kqueue filters");
        unsafe {
            close(notify_fd);
            close(kq);
            notify_cancel(token);
        }
        std::process::exit(1);
    }

    let mut current_state: u64 = 0;
    unsafe { notify_get_state(token, &mut current_state) };

    log!(
        "Started. Initial State: [{}] {}",
        current_state,
        describe_pressure_level(current_state)
    );

    let mut lpm_active = current_state >= 1;
    if lpm_active {
        set_low_power_mode(true);
    }

    let mut event = std::mem::MaybeUninit::<Kevent>::uninit();
    let mut token_buf = [0u8; 4];

    loop {
        // Blocks indefinitely with NULL timeout until the kernel pushes an event
        let n = unsafe {
            kevent(
                kq,
                std::ptr::null(),
                0,
                event.as_mut_ptr(),
                1,
                std::ptr::null(), // NULL = 0 timer wakeups, pure interrupt-driven sleep
            )
        };

        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(4) {
                // EINTR: an unmonitored signal hit the thread, safely re-enter kevent
                continue;
            }
            log!("kevent error: {err}");
            break;
        }

        if n == 0 {
            continue;
        }

        let ev = unsafe { event.assume_init() };

        // Synchronous shutdown signal from launchctl or terminal
        if ev.filter == EVFILT_SIGNAL {
            log!("Received termination signal ({}), shutting down...", ev.ident);
            break;
        }

        // Cooldown timer expired: machine stayed at Level 0 for the required duration
        if ev.filter == EVFILT_TIMER {
            if lpm_active && current_state == 0 {
                set_low_power_mode(false);
                lpm_active = false;
            }
            continue;
        }

        // Thermal event posted to notify_fd
        if ev.filter == EVFILT_READ && ev.ident == notify_fd as usize {
            let bytes_read = unsafe { read(notify_fd, token_buf.as_mut_ptr(), token_buf.len()) };
            if bytes_read <= 0 {
                log!("Notification pipe closed unexpectedly");
                break;
            }

            let mut new_state: u64 = 0;
            let query_status = unsafe { notify_get_state(token, &mut new_state) };

            if query_status == NOTIFY_STATUS_OK && new_state != current_state {
                log!(
                    "Transition: [{}] {} -> [{}] {}",
                    current_state,
                    describe_pressure_level(current_state),
                    new_state,
                    describe_pressure_level(new_state)
                );

                if new_state >= 1 {
                    // Cancel any active cooldown timer and throttle
                    let cancel_timer = Kevent {
                        ident: 1,
                        filter: EVFILT_TIMER,
                        flags: EV_DELETE,
                        fflags: 0,
                        data: 0,
                        udata: std::ptr::null_mut(),
                    };
                    unsafe { kevent(kq, &cancel_timer, 1, std::ptr::null_mut(), 0, std::ptr::null()) };

                    if !lpm_active {
                        set_low_power_mode(true);
                        lpm_active = true;
                    }
                } else if new_state == 0 && lpm_active {
                    // Arm a one-shot cooldown timer instead of immediately disabling LPM
                    let arm_timer = Kevent {
                        ident: 1,
                        filter: EVFILT_TIMER,
                        flags: EV_ADD | EV_ENABLE | EV_ONESHOT,
                        fflags: 0,
                        data: COOLDOWN_MS,
                        udata: std::ptr::null_mut(),
                    };
                    unsafe { kevent(kq, &arm_timer, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
                }

                current_state = new_state;
            }
        }
    }

    // Guaranteed cleanup before exit
    if lpm_active {
        set_low_power_mode(false);
    }

    unsafe {
        close(notify_fd);
        close(kq);
        notify_cancel(token);
    }

    log!("Terminated cleanly.");
    Ok(())
}
