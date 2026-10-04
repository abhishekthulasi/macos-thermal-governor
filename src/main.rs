#[cfg(target_os = "macos")]
mod daemon {
    use std::ffi::{c_char, c_int};
    use std::io::{self, ErrorKind};
    use std::mem::{self, MaybeUninit};
    use std::os::fd::RawFd;
    use std::os::unix::process::CommandExt;
    use std::process::Command;
    use std::ptr;

    const NOTIFY_STATUS_OK: u32 = 0;
    const NOTIFY_KEY: &[u8] = b"com.apple.system.thermalpressurelevel\0";
    const ELEVATED_THRESHOLD: u64 = 1;
    const COOLDOWN_MS: isize = 120_000;
    const COOLDOWN_TIMER_ID: usize = 1;

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

    macro_rules! log {
        ($($arg:tt)*) => {{
            eprintln!("[thermal-lpm] {}", format_args!($($arg)*));
        }};
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Profile {
        Battery,
        Charger,
    }

    impl Profile {
        const ALL: [Self; 2] = [Self::Battery, Self::Charger];

        fn flag(self) -> &'static str {
            match self {
                Self::Battery => "-b",
                Self::Charger => "-c",
            }
        }

        fn name(self) -> &'static str {
            match self {
                Self::Battery => "battery",
                Self::Charger => "charger",
            }
        }
    }

    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    struct LpmSnapshot {
        battery: Option<bool>,
        charger: Option<bool>,
    }

    impl LpmSnapshot {
        fn get(self, profile: Profile) -> Option<bool> {
            match profile {
                Profile::Battery => self.battery,
                Profile::Charger => self.charger,
            }
        }

        fn insert(&mut self, profile: Profile, value: bool) -> io::Result<()> {
            let slot = match profile {
                Profile::Battery => &mut self.battery,
                Profile::Charger => &mut self.charger,
            };

            if slot.replace(value).is_some() {
                return Err(invalid_data(format!(
                    "duplicate lowpowermode value for {} profile",
                    profile.name()
                )));
            }
            Ok(())
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Ownership {
        /// LPM is currently off and this daemon may claim the profile.
        Eligible,
        /// This daemon successfully changed the profile from off to on.
        Daemon,
        /// The profile is user/system managed. Never touch it again this run.
        External,
        /// The profile was not safely observable. Fail closed.
        Unavailable,
    }

    #[derive(Debug)]
    struct LpmController {
        battery: Ownership,
        charger: Ownership,
    }

    impl LpmController {
        fn new(snapshot: LpmSnapshot) -> Self {
            Self {
                battery: Self::initial_state(snapshot.battery),
                charger: Self::initial_state(snapshot.charger),
            }
        }

        fn initial_state(value: Option<bool>) -> Ownership {
            match value {
                Some(false) => Ownership::Eligible,
                Some(true) => Ownership::External,
                None => Ownership::Unavailable,
            }
        }

        fn state(&self, profile: Profile) -> Ownership {
            match profile {
                Profile::Battery => self.battery,
                Profile::Charger => self.charger,
            }
        }

        fn set_state(&mut self, profile: Profile, state: Ownership) {
            match profile {
                Profile::Battery => self.battery = state,
                Profile::Charger => self.charger = state,
            }
        }

        fn owns_any(&self) -> bool {
            Profile::ALL
                .iter()
                .copied()
                .any(|profile| self.state(profile) == Ownership::Daemon)
        }

        fn log_initial_state(&self) {
            for profile in Profile::ALL {
                match self.state(profile) {
                    Ownership::Eligible => {
                        log!("{} profile is eligible for daemon control", profile.name());
                    }
                    Ownership::External => {
                        log!(
                            "{} profile already has LPM enabled; leaving it externally managed",
                            profile.name()
                        );
                    }
                    Ownership::Unavailable => {
                        log!(
                            "{} profile is not safely observable; it will not be modified",
                            profile.name()
                        );
                    }
                    Ownership::Daemon => unreachable!(),
                }
            }
        }

        fn enable_for_eligible_profiles(&mut self) {
            let snapshot = match query_lpm_state() {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    log!("Cannot verify LPM state; no settings changed: {error}");
                    return;
                }
            };

            for profile in Profile::ALL {
                let observed = snapshot.get(profile);

                match (self.state(profile), observed) {
                    // If our expected ON value became OFF, something external won.
                    // Relinquish the profile and never fight the user's setting.
                    (Ownership::Daemon, Some(false)) => {
                        log!(
                            "{} profile changed externally; relinquishing daemon ownership",
                            profile.name()
                        );
                        self.set_state(profile, Ownership::External);
                    }
                    (Ownership::Daemon, _) => {}

                    // A profile that becomes ON before we claim it is external.
                    (Ownership::Eligible, Some(true)) => {
                        log!(
                            "{} profile was enabled externally; leaving it untouched",
                            profile.name()
                        );
                        self.set_state(profile, Ownership::External);
                    }
                    (Ownership::Eligible, Some(false)) => self.claim(profile),
                    (Ownership::Eligible, None) => {
                        self.set_state(profile, Ownership::Unavailable);
                    }

                    // External ownership is sticky for the lifetime of the process.
                    (Ownership::External, _) => {}

                    // A profile can appear later (hardware/output differences). Re-evaluate
                    // it, but still never take over a value that is already ON.
                    (Ownership::Unavailable, Some(true)) => {
                        self.set_state(profile, Ownership::External);
                    }
                    (Ownership::Unavailable, Some(false)) => {
                        self.set_state(profile, Ownership::Eligible);
                        self.claim(profile);
                    }
                    (Ownership::Unavailable, None) => {}
                }
            }
        }

        fn claim(&mut self, profile: Profile) {
            match query_lpm_state()
                .ok()
                .and_then(|snapshot| snapshot.get(profile))
            {
                Some(false) => {}
                Some(true) => {
                    self.set_state(profile, Ownership::External);
                    log!(
                        "{} profile was enabled externally before claiming it; \
                         leaving it untouched",
                        profile.name()
                    );
                    return;
                }
                None => {
                    self.set_state(profile, Ownership::Unavailable);
                    log!(
                        "{} profile could not be verified immediately before claiming; \
                         leaving it untouched",
                        profile.name()
                    );
                    return;
                }
            }

            match set_lpm(profile, true) {
                Ok(()) => {
                    match query_lpm_state().ok().and_then(|snapshot| snapshot.get(profile)) {
                        Some(true) => {
                            self.set_state(profile, Ownership::Daemon);
                            log!("LPM enabled for {} profile (daemon-owned)", profile.name());
                        }
                        Some(false) => {
                            self.set_state(profile, Ownership::External);
                            log!(
                                "{} profile was changed externally while claiming it; \
                                 relinquishing ownership",
                                profile.name()
                            );
                        }
                        None => {
                            self.set_state(profile, Ownership::Daemon);
                            log!(
                                "{} profile could not be verified after enabling; retaining \
                                 daemon ownership for safe restoration",
                                profile.name()
                            );
                        }
                    }
                }
                Err(error) => {
                    log!("Failed to enable LPM for {} profile: {error}", profile.name());
                }
            }
        }

        fn restore_owned(&mut self, reason: &str) {
            if !self.owns_any() {
                return;
            }

            let snapshot = match query_lpm_state() {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    if reason == "shutdown" {
                        log!(
                            "Cannot verify LPM state during shutdown; attempting direct \
                             restoration of daemon-owned profiles: {error}"
                        );
                        for profile in Profile::ALL {
                            if self.state(profile) != Ownership::Daemon {
                                continue;
                            }

                            match set_lpm(profile, false) {
                                Ok(()) => {
                                    self.set_state(profile, Ownership::Eligible);
                                    log!(
                                        "Restored {} profile after shutdown using daemon \
                                         ownership record",
                                        profile.name()
                                    );
                                }
                                Err(error) => {
                                    log!(
                                        "Failed to restore {} profile during shutdown: {error}",
                                        profile.name()
                                    );
                                }
                            }
                        }
                    } else {
                        log!(
                            "Cannot verify LPM state during {reason}; no settings changed: {error}"
                        );
                    }
                    return;
                }
            };

            for profile in Profile::ALL {
                if self.state(profile) != Ownership::Daemon {
                    continue;
                }

                match snapshot.get(profile) {
                    Some(true) => match set_lpm(profile, false) {
                        Ok(()) => {
                            self.set_state(profile, Ownership::Eligible);
                            log!("Restored {} profile after {reason}", profile.name());
                        }
                        Err(error) => {
                            log!("Failed to restore {} profile: {error}", profile.name());
                        }
                    },
                    Some(false) => {
                        // Someone else already changed it. Do not write over them.
                        self.set_state(profile, Ownership::External);
                        log!(
                            "{} profile changed externally before {reason}; not overwriting it",
                            profile.name()
                        );
                    }
                    None => {
                        // Fail closed. Keep the ownership record so a later retry can
                        // restore it if the profile becomes observable again.
                        log!(
                            "{} profile is unobservable during {reason}; not writing",
                            profile.name()
                        );
                    }
                }
            }
        }
    }

    fn invalid_data(message: impl Into<String>) -> io::Error {
        io::Error::new(ErrorKind::InvalidData, message.into())
    }

    fn parse_lpm_snapshot(text: &str) -> io::Result<LpmSnapshot> {
        let mut snapshot = LpmSnapshot::default();
        let mut section = None;

        for raw_line in text.lines() {
            let line = raw_line.trim();

            // Reset on *every* power section. This prevents an unknown section such
            // as UPS Power from being mistaken for the previous Battery/AC section.
            if line.ends_with("Power:") {
                section = match line {
                    "Battery Power:" => Some(Profile::Battery),
                    "AC Power:" | "Charger Power:" => Some(Profile::Charger),
                    _ => None,
                };
                continue;
            }

            let Some(profile) = section else {
                continue;
            };

            let mut fields = line.split_whitespace();
            if fields.next() != Some("lowpowermode") {
                continue;
            }

            let value = match fields.next() {
                Some("0") => false,
                Some("1") => true,
                Some(value) => {
                    return Err(invalid_data(format!(
                        "unexpected lowpowermode value {value:?} in {} profile",
                        profile.name()
                    )));
                }
                None => {
                    return Err(invalid_data(format!(
                        "missing lowpowermode value in {} profile",
                        profile.name()
                    )));
                }
            };

            snapshot.insert(profile, value)?;
        }

        if snapshot.battery.is_none() && snapshot.charger.is_none() {
            return Err(invalid_data(
                "pmset output contained no supported lowpowermode profile",
            ));
        }

        Ok(snapshot)
    }

    fn query_lpm_state() -> io::Result<LpmSnapshot> {
        let output = Command::new("/usr/bin/pmset")
            .args(["-g", "custom"])
            .output()?;

        if !output.status.success() {
            return Err(io::Error::other(format!(
                "pmset -g custom failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }

        let stdout = String::from_utf8(output.stdout)
            .map_err(|error| invalid_data(format!("pmset returned invalid UTF-8: {error}")))?;
        parse_lpm_snapshot(&stdout)
    }

    fn set_lpm(profile: Profile, enabled: bool) -> io::Result<()> {
        let value = if enabled { "1" } else { "0" };
        let output = Command::new("/usr/bin/pmset")
            .args([profile.flag(), "lowpowermode", value])
            .output()?;

        if output.status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "pmset {} lowpowermode {value} failed ({}): {}",
                profile.flag(),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )))
        }
    }

    pub fn ensure_power_mode_permissions() -> io::Result<()> {
        if unsafe { libc::geteuid() } == 0 {
            return Ok(());
        }

        eprintln!("[thermal-lpm] Administrator privileges are required to change Low Power Mode.");
        eprint!("[thermal-lpm] Restart automatically with administrator privileges? [y/n] ");
        io::Write::flush(&mut io::stderr())?;

        let mut answer = String::new();
        loop {
            answer.clear();
            if io::stdin().read_line(&mut answer)? == 0 {
                return Err(io::Error::new(
                    ErrorKind::UnexpectedEof,
                    "cannot ask for administrator privileges without an interactive terminal",
                ));
            }

            match answer.trim().to_ascii_lowercase().as_str() {
                "y" | "yes" => break,
                "n" | "no" => {
                    eprintln!(
                        "[thermal-lpm] Administrator privileges are required; exiting."
                    );
                    return Err(io::Error::new(
                        ErrorKind::PermissionDenied,
                        "administrator privileges declined",
                    ));
                }
                _ => {
                    eprint!("[thermal-lpm] Please answer y or n: ");
                    io::Write::flush(&mut io::stderr())?;
                }
            }
        }

        let executable = std::env::current_exe()?;
        let arguments: Vec<_> = std::env::args_os().skip(1).collect();
        eprintln!("[thermal-lpm] Restarting with administrator privileges...");

        let error = Command::new("/usr/bin/sudo")
            .arg(executable)
            .args(arguments)
            .exec();
        Err(error)
    }

    struct NotifyRegistration {
        fd: RawFd,
        token: c_int,
    }

    impl NotifyRegistration {
        fn new() -> io::Result<Self> {
            let mut fd = -1;
            let mut token = 0;
            let status = unsafe {
                // SAFETY: NOTIFY_KEY is NUL-terminated; fd/token are writable.
                notify_register_file_descriptor(
                    NOTIFY_KEY.as_ptr().cast::<c_char>(),
                    &mut fd,
                    0,
                    &mut token,
                )
            };

            if status != NOTIFY_STATUS_OK || fd < 0 {
                if fd >= 0 {
                    unsafe { libc::close(fd) };
                }
                return Err(io::Error::other(format!(
                    "notify registration failed with status {status}"
                )));
            }

            Ok(Self { fd, token })
        }

        fn state(&self) -> io::Result<u64> {
            let mut state = 0_u64;
            let status = unsafe {
                // SAFETY: token is valid while self is alive; state is writable.
                notify_get_state(self.token, &mut state)
            };

            if status == NOTIFY_STATUS_OK {
                Ok(state)
            } else {
                Err(io::Error::other(format!(
                    "notify_get_state failed with status {status}"
                )))
            }
        }

        fn read_token(&self) -> io::Result<c_int> {
            // notify(3) writes the registration token to the descriptor in network
            // byte order. Read it as a u32 and explicitly convert to host order
            // before comparing it with the token returned at registration time.
            let mut network_token = 0_u32;
            let size = mem::size_of::<u32>();

            loop {
                let bytes_read = unsafe {
                    // SAFETY: network_token is writable for exactly `size` bytes;
                    // self.fd remains valid while this registration is alive.
                    libc::read(
                        self.fd,
                        (&mut network_token as *mut u32).cast(),
                        size,
                    )
                };

                if bytes_read == size as isize {
                    let host_token = u32::from_be(network_token);
                    if host_token > c_int::MAX as u32 {
                        return Err(invalid_data(format!(
                            "notification token {host_token} does not fit in c_int"
                        )));
                    }
                    return Ok(host_token as c_int);
                }
                if bytes_read == 0 {
                    return Err(io::Error::new(
                        ErrorKind::UnexpectedEof,
                        "notification descriptor closed",
                    ));
                }
                if bytes_read < 0 {
                    let error = io::Error::last_os_error();
                    if error.raw_os_error() == Some(libc::EINTR) {
                        continue;
                    }
                    return Err(error);
                }
                return Err(invalid_data(format!(
                    "short notification read: expected {size} bytes, got {bytes_read}"
                )));
            }
        }
    }

    impl Drop for NotifyRegistration {
        fn drop(&mut self) {
            // notify_cancel() releases the file descriptor associated with the
            // registration. Only close it ourselves if cancellation fails.
            let status = unsafe { notify_cancel(self.token) };
            if status != NOTIFY_STATUS_OK {
                unsafe {
                    let _ = libc::close(self.fd);
                }
            }
        }
    }

    struct Kqueue {
        fd: RawFd,
    }

    impl Kqueue {
        fn new() -> io::Result<Self> {
            let fd = unsafe { libc::kqueue() };
            if fd < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(Self { fd })
            }
        }

        fn register(&self, notify_fd: RawFd) -> io::Result<()> {
            let changes = [
                event(
                    notify_fd as usize,
                    libc::EVFILT_READ,
                    libc::EV_ADD | libc::EV_ENABLE,
                    0,
                ),
                event(
                    libc::SIGTERM as usize,
                    libc::EVFILT_SIGNAL,
                    libc::EV_ADD | libc::EV_ENABLE,
                    0,
                ),
                event(
                    libc::SIGINT as usize,
                    libc::EVFILT_SIGNAL,
                    libc::EV_ADD | libc::EV_ENABLE,
                    0,
                ),
            ];

            let result = unsafe {
                // SAFETY: changes is initialized and lives through the call.
                libc::kevent(
                    self.fd,
                    changes.as_ptr(),
                    changes.len() as c_int,
                    ptr::null_mut(),
                    0,
                    ptr::null(),
                )
            };

            if result < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        }

        fn wait(&self) -> io::Result<libc::kevent> {
            let mut output = MaybeUninit::<libc::kevent>::uninit();

            loop {
                let result = unsafe {
                    // SAFETY: output has room for one event. Null timeout blocks.
                    libc::kevent(
                        self.fd,
                        ptr::null(),
                        0,
                        output.as_mut_ptr(),
                        1,
                        ptr::null(),
                    )
                };

                if result > 0 {
                    let event = unsafe { output.assume_init() };
                    // `libc::kevent` is packed on Darwin. Copy potentially
                    // unaligned fields before using them in ordinary Rust code.
                    let flags = event.flags;
                    let data = event.data;
                    if flags & libc::EV_ERROR != 0 && data != 0 {
                        return Err(io::Error::from_raw_os_error(data as i32));
                    }
                    return Ok(event);
                }
                if result == 0 {
                    continue;
                }

                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::EINTR) {
                    return Err(error);
                }
            }
        }

        fn arm_cooldown(&self) -> io::Result<()> {
            self.submit(event(
                COOLDOWN_TIMER_ID,
                libc::EVFILT_TIMER,
                libc::EV_ADD | libc::EV_ENABLE | libc::EV_ONESHOT,
                COOLDOWN_MS,
            ))
        }

        fn cancel_cooldown(&self) -> io::Result<()> {
            match self.submit(event(
                COOLDOWN_TIMER_ID,
                libc::EVFILT_TIMER,
                libc::EV_DELETE,
                0,
            )) {
                Ok(()) => Ok(()),
                Err(error) if error.raw_os_error() == Some(libc::ENOENT) => Ok(()),
                Err(error) => Err(error),
            }
        }

        fn submit(&self, change: libc::kevent) -> io::Result<()> {
            let result = unsafe {
                libc::kevent(
                    self.fd,
                    &change,
                    1,
                    ptr::null_mut(),
                    0,
                    ptr::null(),
                )
            };
            if result < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        }
    }

    impl Drop for Kqueue {
        fn drop(&mut self) {
            unsafe {
                let _ = libc::close(self.fd);
            }
        }
    }

    fn event(ident: usize, filter: i16, flags: u16, data: isize) -> libc::kevent {
        libc::kevent {
            ident,
            filter,
            flags,
            fflags: 0,
            data,
            udata: ptr::null_mut(),
        }
    }

    extern "C" fn signal_handler(_: c_int) {}

    fn install_signal_handler(signal: c_int) -> io::Result<()> {
        let mut action: libc::sigaction = unsafe { mem::zeroed() };
        action.sa_sigaction = signal_handler as libc::sighandler_t;
        action.sa_flags = 0;

        if unsafe { libc::sigemptyset(&mut action.sa_mask) } != 0
            || unsafe { libc::sigaction(signal, &action, ptr::null_mut()) } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn pressure_name(level: u64) -> &'static str {
        match level {
            0 => "nominal",
            1 => "elevated level 1",
            2 => "level 2",
            3 => "level 3",
            4 => "level 4",
            _ => "unknown level",
        }
    }

    pub fn run() -> io::Result<()> {
        install_signal_handler(libc::SIGTERM)?;
        install_signal_handler(libc::SIGINT)?;

        let notify = NotifyRegistration::new()?;
        let kqueue = Kqueue::new()?;
        kqueue.register(notify.fd)?;

        let mut controller = LpmController::new(query_lpm_state()?);
        controller.log_initial_state();

        let mut pressure = notify.state()?;
        log!("Started at thermal pressure {} ({pressure})", pressure_name(pressure));

        if pressure >= ELEVATED_THRESHOLD {
            controller.enable_for_eligible_profiles();
        }

        let mut cooldown_armed = false;

        loop {
            let ev = match kqueue.wait() {
                Ok(event) => event,
                Err(error) => {
                    log!("kqueue wait failed: {error}");
                    break;
                }
            };

            // `libc::kevent` is packed on Darwin, so copy fields into aligned
            // locals before comparisons/formatting.
            let ev_filter = ev.filter;
            let ev_ident = ev.ident;

            if ev_filter == libc::EVFILT_SIGNAL {
                log!("Received termination signal {ev_ident}; shutting down");
                break;
            }

            if ev_filter == libc::EVFILT_TIMER && ev_ident == COOLDOWN_TIMER_ID {
                cooldown_armed = false;
                if pressure == 0 {
                    controller.restore_owned("cooldown");

                    // Retry transient query/write failures instead of stranding a
                    // daemon-owned setting while the machine stays nominal.
                    if controller.owns_any() {
                        match kqueue.arm_cooldown() {
                            Ok(()) => cooldown_armed = true,
                            Err(error) => log!("Failed to arm restore retry: {error}"),
                        }
                    }
                }
                continue;
            }

            if ev_filter != libc::EVFILT_READ || ev_ident != notify.fd as usize {
                continue;
            }

            let delivered_token = match notify.read_token() {
                Ok(token) => token,
                Err(error) => {
                    log!("Notification read failed: {error}");
                    break;
                }
            };

            if delivered_token != notify.token {
                log!("Ignoring unexpected notification token {delivered_token}");
                continue;
            }

            let next = match notify.state() {
                Ok(value) => value,
                Err(error) => {
                    log!("Thermal state query failed: {error}");
                    continue;
                }
            };

            if next == pressure {
                continue;
            }

            log!(
                "Thermal transition: {} ({pressure}) -> {} ({next})",
                pressure_name(pressure),
                pressure_name(next)
            );

            if next >= ELEVATED_THRESHOLD {
                if cooldown_armed {
                    match kqueue.cancel_cooldown() {
                        Ok(()) => cooldown_armed = false,
                        Err(error) => log!("Failed to cancel cooldown: {error}"),
                    }
                }
                controller.enable_for_eligible_profiles();
            } else if controller.owns_any() && !cooldown_armed {
                match kqueue.arm_cooldown() {
                    Ok(()) => {
                        cooldown_armed = true;
                        log!("Nominal state reached; cooldown armed");
                    }
                    Err(error) => log!("Failed to arm cooldown: {error}"),
                }
            }

            pressure = next;
        }

        if cooldown_armed {
            if let Err(error) = kqueue.cancel_cooldown() {
                log!("Failed to cancel cooldown during shutdown: {error}");
            }
        }

        controller.restore_owned("shutdown");
        log!("Terminated cleanly");
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parses_known_profiles() {
            let input = "Battery Power:\n lowpowermode 0\nAC Power:\n lowpowermode 1\n";
            let result = parse_lpm_snapshot(input).unwrap();
            assert_eq!(result.battery, Some(false));
            assert_eq!(result.charger, Some(true));
        }

        #[test]
        fn unknown_section_does_not_leak_into_previous_profile() {
            let input = concat!(
                "Battery Power:\n lowpowermode 0\n",
                "UPS Power:\n lowpowermode 1\n",
                "AC Power:\n lowpowermode 0\n"
            );
            let result = parse_lpm_snapshot(input).unwrap();
            assert_eq!(result.battery, Some(false));
            assert_eq!(result.charger, Some(false));
        }

        #[test]
        fn accepts_charger_header_alias() {
            let input = "Charger Power:\n lowpowermode 1\n";
            let result = parse_lpm_snapshot(input).unwrap();
            assert_eq!(result.charger, Some(true));
        }

        #[test]
        fn rejects_invalid_low_power_value() {
            assert!(parse_lpm_snapshot("Battery Power:\n lowpowermode 7\n").is_err());
        }
    }
}

#[cfg(target_os = "macos")]
fn main() {
    if let Err(error) = daemon::ensure_power_mode_permissions().and_then(|()| daemon::run()) {
        eprintln!("[thermal-lpm] fatal error: {error}");
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("thermal-lpm-daemon supports macOS only");
    std::process::exit(1);
}
