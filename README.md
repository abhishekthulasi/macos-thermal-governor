# macOS Thermal Governor

A lightweight background daemon for Apple Silicon Macs that automatically enables Low Power Mode during elevated thermal pressure and smoothly restores standard performance once the machine cools down.

Built in pure Rust using native Darwin kernel primitives (`kqueue` and `notify(3)`), it sleeps with zero timer wakeups and consumes 0.0% CPU while idle.

---

## Features

* **Zero Idle Overhead:** Interrupt-driven event loop via BSD `kqueue` and Darwin notifications. No polling, no periodic timers, and no battery drain.
* **Proactive Thermal Mitigation:** Automatically triggers Low Power Mode via `pmset` the moment the OS signals moderate thermal throttling.
* **Hysteresis & Cooldown:** Implements a built-in 120-second cooldown timer before restoring standard clock speeds, preventing rapid power-state oscillation.
* **Graceful Lifecycle:** Cleanly intercepts termination signals (`SIGTERM`, `SIGINT`) to guarantee Low Power Mode is disabled before exiting.
* **Native LaunchDaemon:** Runs in the background as a managed system service that starts automatically on boot.

---

## How It Works

1. Listens for kernel notifications on `com.apple.system.thermalpressurelevel`.
2. When pressure rises to **Moderate** (Level 1) or higher:
   * Immediately activates Low Power Mode (`pmset -a lowpowermode 1`).
   * Cancels any pending cooldown timers.
3. When pressure drops back to **Nominal** (Level 0):
   * Arms a one-shot 120-second timer.
   * If the system remains cool for the full duration, it turns off Low Power Mode (`pmset -a lowpowermode 0`).

---

## System Requirements

* Apple Silicon Mac (M1 or later)
* macOS 12 Monterey or later
* Administrator privileges (required by macOS to adjust power settings via `pmset`)

---

## Installation

### Option 1: Installer Package (Recommended)

1. Download `macos-thermal-governor-0.1.0.pkg` from the [Latest Release](../../releases/latest).
2. Because the package is not notarized through Apple's paid developer program, macOS Gatekeeper will block a standard double-click. Run this in Terminal to clear the quarantine flag:
   ```bash
   xattr -d com.apple.quarantine ~/Downloads/macos-thermal-governor-0.1.0.pkg
   ```
   *Alternatively:* Right-click (Control-click) the `.pkg` file, choose **Open**, and click **Open** in the prompt.
3. Follow the installation wizard. The daemon will automatically register and start running in the background.

---

### Option 2: Build From Source

Prerequisites: Rust toolchain installed (`rustup`).

1. Clone the repository:
   ```bash
   git clone https://github.com/abhishekthulasi/macos-thermal-governor.git
   cd macos-thermal-governor
   ```

2. Build the optimized release binary:
   ```bash
   cargo build --release
   ```

3. Copy the binary to your system path:
   ```bash
   sudo cp target/release/macos-thermal-governor /usr/local/bin/
   sudo chmod 755 /usr/local/bin/macos-thermal-governor
   ```

4. Install and start the LaunchDaemon:
   ```bash
   sudo cp com.local.macos-thermal-governor.plist /Library/LaunchDaemons/
   sudo chown root:wheel /Library/LaunchDaemons/com.local.macos-thermal-governor.plist
   sudo chmod 644 /Library/LaunchDaemons/com.local.macos-thermal-governor.plist
   sudo launchctl bootstrap system /Library/LaunchDaemons/com.local.macos-thermal-governor.plist
   ```

---

## Verifying & Logs

To check current daemon activity and thermal state transitions, stream the log file:

```bash
tail -f /var/log/macos-thermal-governor.log
```

Example output:
```text
[2026-03-29 10:14:02 UTC] Started. Initial State: [0] Nominal (Full performance)
[2026-03-29 10:32:15 UTC] Transition: [0] Nominal (Full performance) -> [1] Moderate (Minor throttling / increased heat)
[2026-03-29 10:32:15 UTC] Low Power Mode -> 1
[2026-03-29 10:38:40 UTC] Transition: [1] Moderate (Minor throttling / increased heat) -> [0] Nominal (Full performance)
[2026-03-29 10:40:40 UTC] Low Power Mode -> 0
```

---

## Uninstallation

If you installed via the `.pkg`, run the built-in uninstaller helper:

```bash
sudo macos-thermal-governor-uninstall
```

If you installed manually from source:

```bash
sudo launchctl bootout system/com.local.macos-thermal-governor 2>/dev/null || true
sudo rm -f /Library/LaunchDaemons/com.local.macos-thermal-governor.plist
sudo rm -f /usr/local/bin/macos-thermal-governor
sudo rm -f /var/log/macos-thermal-governor.log
sudo /usr/bin/pmset -a lowpowermode 0
```

---

## License

This project is licensed under the [MIT License](LICENSE).
