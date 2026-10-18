# Windows hardening run — 2026-10-18

Host: Windows 11 Home x64, build 26300, AMD Radeon RX 9070 XT, MSI MPG
274URDFW E16M display, and a SudoMaker virtual display adapter. Built natively
with Rust 1.99.1, Visual Studio Build Tools 2022 and the Windows SDK.

Every timer completion used `NIGHTCAP_DRY_RUN=1` or `DryRunPower`. No actual
suspend, hibernation or shutdown was requested. Real audio started at 58% and
monitor brightness at 10%; both were verified restored after hardware tests.

## Reproduced issues and fixes

- **Startup access violation:** launching without an existing COM apartment
  crashed with `0xc0000005`. Native symbol resolution located the fault in
  `windows_result::com` destruction. WMI errors retained COM error information
  past `CoUninitialize`. Native calls now stringify and release errors before
  apartment teardown. Both fresh-thread discovery and existing-STA/worker
  transfer regressions pass. Microsoft documents that
  [CoUninitialize unloads COM resources](https://learn.microsoft.com/en-us/windows/win32/api/combaseapi/nf-combaseapi-couninitialize).
- **Power privilege checks:** a native acquisition-only regression failed with
  `0x800703E6`. `AdjustTokenPrivileges` received `PreviousState` without its
  required `ReturnLength` pointer. Acquisition and restoration of the process
  token now pass without performing a power action. See the
  [API contract](https://learn.microsoft.com/en-us/windows/win32/api/securitybaseapi/nf-securitybaseapi-adjusttokenprivileges).
- **Frozen native UI and unavailable transparency:** the default Windows WGPU
  renderer painted the initial frame but became unresponsive, and logged that
  transparent surfaces were unsupported. Selecting Glow on Windows restored
  responsive controls, accessibility and transparent overlay rendering.
- **Audio output routing:** writes previously resolved the current default
  output rather than the captured output. A successful capture now retains
  its endpoint ID; subsequent writes and restore retries reopen that endpoint.
  Injected-device tests verify switching, disconnect failure, retry and the
  next capture selecting the new default without changing real OS routing.
- **Partial display failures:** capture no longer requires both WMI and DDC
  to remain available. Restoration attempts all captured providers and monitors
  even after a failure, and missing WMI identities keep restoration pending.
  Deterministic tests cover missing/failed devices and untouched new devices.
- **Silently ignored brightness writes:** this MSI display reported brightness
  range 0–100 and current 10. Both high-level DDC and VCP writes to 5 returned
  success, but read-back remained 10, including after a one-second delay; a
  request for zero was also ignored. DDC writes now verify read-back, allow
  one native step or 1% tolerance, retry once after 100 ms, then report failure
  so the engine uses the overlay. The original level remains available for
  restoration. Working native hardware dimming was not established on this host.
- **Fallback continuity:** native failure originally converted a roughly 10%
  hardware level directly into an approximately 83% opaque overlay. A failing
  regression observed 0.8115 opacity instead of 0.015 at the first fade step.
  Overlay opacity now scales relative to the last successful native level;
  the continuity and restoration regression passes.

## Executed verification

```powershell
cargo test --locked --all-targets
cargo test --locked --test windows_runtime -- --ignored --nocapture --test-threads=1
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo build --release --locked
```

- Default suite: 91 passed, zero failures; one hardware test intentionally ignored.
- Opt-in hardware suite: one passed. Real volume fell from 0.58 to 0.435;
  unsupported hardware dimming transitioned to overlay opacity 0.200 rather
  than 0.830. Pause/resume, extension, cancel, synthetic wake, restoration off,
  worker shutdown and dry-run Sleep/Hibernate/Shutdown dispatch were exercised.
- Native GUI: Start/Pause, a frozen paused countdown, Cancel, custom one-minute
  keyboard entry followed directly by Start, disabled running duration controls,
  separate Settings open/close, independent fade toggles, JSON persistence,
  accelerated completion and graceful quit from finishing were exercised.
  Sleep, Hibernate and Shut Down were visible in the native action picker.
- Native overlay: a real transparent fullscreen viewport appeared after the
  monitor ignored writes. Cancel remained clickable through the overlay and
  removed it. Main controls exposed native accessible names and roles.
- Formatting, Clippy and the optimized Windows build passed. The small pixel
  slice changes accommodate the current stable Clippy checks used by Windows CI.
- Packaged and extracted the release ZIP with the executable, README and both
  licenses. Its PE subsystem is Windows GUI (2). The extracted executable
  launched responsively; an accelerated timer completed with a dry-run log while
  its root window was hidden, confirming close-to-tray background operation.

Windows pull requests now run formatting, all default tests, Clippy and an
optimized build before release. The hardware test remains opt-in because it
changes real levels and needs stable devices; it never calls native power actions.

## Remaining limits

- Windows 10, other GPUs, native WMI panels and a monitor that applies DDC
  brightness writes were not physically exercised.
- Actual sleep, hibernate, shutdown, wake delivery and recovery from physical
  display/audio disconnection still need supervised hardware testing.
- Multi-device routing and partial-failure cases use deterministic injected
  tests; this host had one active physical monitor. DDC handle recovery after
  unplug/replug is not guaranteed.
- Native tray initialization and automated command dispatch are covered, but
  the full notification-area menu was not clicked through in this run.
- Native dark appearance was inspected; native light appearance was not.
- The Settings viewport responds to pointer/keyboard input, but its native
  accessibility tree exposes window chrome rather than the settings controls.
  The main-window accessibility tree is available.
