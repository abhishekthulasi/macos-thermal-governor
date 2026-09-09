use std::ffi::{c_char, c_int};
use std::fs::File;
use std::io::{Read};
use std::os::fd::{FromRawFd, IntoRawFd};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

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

extern "C" fn handle_sigterm(_: c_int) {
    RUNNING.store(false, Ordering::SeqCst);
}

fn set_low_power_mode(enable: bool) {
    let val = if enable { "1" } else { "0" };
    let status = Command::new("/usr/bin/pmset")
        .args(["-a", "lowpowermode", val])
        .status();

    match status {
        Ok(s) if s.success() => eprintln!("[ThermalDaemon] Low Power Mode -> {val}"),
        Ok(s) => eprintln!("[ThermalDaemon] pmset exited with status: {s}"),
        Err(e) => eprintln!("[ThermalDaemon] Failed to invoke pmset: {e}"),
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
        eprintln!("[ThermalDaemon] Failed to register notification listener: {status}");
        std::process::exit(1);
    }

    let mut current_state: u64 = 0;
    unsafe { notify_get_state(token, &mut current_state) };

    eprintln!(
        "[ThermalDaemon] Started. Initial State: [{}] {}",
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
            eprintln!(
                "[ThermalDaemon] Transition: [{}] {} -> [{}] {}",
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
    eprintln!("[ThermalDaemon] Terminated cleanly.");

    Ok(())
}
