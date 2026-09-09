use std::ffi::{c_char, c_int};
use std::fs::File;
use std::io::Read;
use std::os::fd::{FromRawFd, IntoRawFd};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const NOTIFY_STATUS_OK: u32 = 0;
const NOTIFY_KEY: &[u8] = b"com.apple.system.thermalpressurelevel\0";

static RUNNING: AtomicBool = AtomicBool::new(true);

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
    fn signal(sig: c_int, handler: extern "C" fn(c_int)) -> usize;
}

/// Formats current UTC time as YYYY-MM-DD HH:MM:SS using standard library time.
fn format_timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let sec = secs % 60;
    let min = (secs / 60) % 60;
    let hour = (secs / 3600) % 24;

    // Convert epoch days to Gregorian date (civil calendar algorithm)
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

extern "C" fn handle_sigterm(_: c_int) {
    RUNNING.store(false, Ordering::SeqCst);
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
    // Intercept SIGTERM from launchd for a graceful exit
    unsafe {
        signal(15, handle_sigterm); // 15 = SIGTERM
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

    let mut current_state: u64 = 0;
    unsafe { notify_get_state(token, &mut current_state) };

    log!(
        "Started. Initial State: [{}] {}",
        current_state,
        describe_pressure_level(current_state)
    );

    // Initial state evaluation
    let mut lpm_active = current_state >= 2;
    if lpm_active {
        set_low_power_mode(true);
    }

    let mut stream = unsafe { File::from_raw_fd(notify_fd) };
    let mut token_buf = [0u8; 4];

    while RUNNING.load(Ordering::SeqCst) && stream.read_exact(&mut token_buf).is_ok() {
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

            let should_enable_lpm = new_state >= 2;
            if should_enable_lpm != lpm_active {
                set_low_power_mode(should_enable_lpm);
                lpm_active = should_enable_lpm;
            }

            current_state = new_state;
        }
    }

    // Cleanup when stopping the daemon
    if lpm_active {
        set_low_power_mode(false);
    }

    let _ = stream.into_raw_fd();
    unsafe { notify_cancel(token) };
    log!("Terminated cleanly.");

    Ok(())
}
