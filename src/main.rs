use std::ffi::{c_char, c_int};
use std::fs::File;
use std::io::Read;
use std::os::fd::{FromRawFd, IntoRawFd};

const NOTIFY_STATUS_OK: u32 = 0;
const NOTIFY_KEY: &[u8] = b"com.apple.system.thermalpressurelevel\0";

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
}

fn describe_pressure_level(level: u64) -> &'static str {
    match level {
        0 => "Nominal (Full performance, no throttling)",
        1 => "Moderate (Minor throttling / increased heat)",
        2 => "Heavy (Significant CPU/GPU frequency throttling)",
        3 => "Trapping (Extreme emergency mitigation)",
        4 => "Sleeping (Forced sleep to prevent hardware damage)",
        _ => "Unknown state",
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
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
        eprintln!("Failed to register notification listener (code: {status})");
        std::process::exit(1);
    }

    // Query and display the baseline state
    let mut current_state: u64 = 0;
    unsafe {
        notify_get_state(token, &mut current_state);
    }

    println!("Monitoring M4 thermal pressure. Press Ctrl+C to exit.");
    println!(
        "Initial State: [{}] {}",
        current_state,
        describe_pressure_level(current_state)
    );

    // Wrap the raw descriptor into a File to block on reads
    let mut stream = unsafe { File::from_raw_fd(notify_fd) };
    let mut token_buf = [0u8; 4];

    // macOS writes a 4-byte token to the descriptor on every state transition
    while stream.read_exact(&mut token_buf).is_ok() {
        let mut new_state: u64 = 0;
        let query_status = unsafe { notify_get_state(token, &mut new_state) };

        if query_status == NOTIFY_STATUS_OK && new_state != current_state {
            println!(
                "Thermal Transition: [{}] {} -> [{}] {}",
                current_state,
                describe_pressure_level(current_state),
                new_state,
                describe_pressure_level(new_state)
            );
            current_state = new_state;
        }
    }

    let _ = stream.into_raw_fd();
    unsafe { notify_cancel(token) };
    Ok(())
}
