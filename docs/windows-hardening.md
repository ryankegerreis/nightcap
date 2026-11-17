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
  A captured output that is no longer present or active is reported as
  disconnected. The engine then drops its restore instead of retrying it, so
  the next timer captures and fades the new default output.
- **Partial display failures:** capture no longer requires both WMI and DDC
  to remain available. Each display records the value Nightcap last wrote.
  A display that fails sits out the rest of the fade while the others keep
  fading natively. Only a failure on every display switches to the overlay.
  Restoration retries the displays that failed and skips displays that are
  already restored, so a retry does not overwrite a display the user has
  adjusted since. Missing WMI identities keep restoration pending. Retries
  stop after about two minutes. Deterministic tests cover missing/failed
  devices and untouched new devices.
- **Silently ignored brightness writes:** this MSI display reported brightness
  range 0–100 and current 10. Both high-level DDC and VCP writes to 5 returned
  success, but read-back remained 10, including after a one-second delay; a
  request for zero was also ignored. DDC writes now verify read-back, allow
  one native step or 1% tolerance, retry once after 100 ms (also when the
  first read-back fails), then report failure so the engine uses the overlay. The original level remains available for
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

# Windows hardening run — 2026-11-17

Same host, now with the SudoMaker virtual display active as a second monitor
(DDC/CI reports it at brightness 0 of 100). Rust 1.99.1. The app was launched for
real in debug and release builds and driven through UI Automation, the real
notification-area icon and menu, and real mouse input. Every timer used
`NIGHTCAP_DRY_RUN=1`; no power action was requested. Audio started at 34.4% and
was verified restored; the test settings file was removed afterwards.

## Reproduced issues and fixes

- **Tray actions and the window froze after the tray menu:** a redraw requested
  as the menu's modal loop started was never delivered. egui then counted the
  repaint as pending and dropped every later request, so a visible window
  stopped updating (a running countdown froze) and tray Quit, Settings… and Open
  Nightcap stayed queued until the pointer moved over the window. Quit therefore
  appeared to do nothing and did not restore volume. An external
  `RDW_INTERNALPAINT` released the queue, which confirmed the cause. Root wakes
  from tray actions and later launches now also post a native paint; engine
  wakes do so only when no root pass ran since the previous wake. Opening and
  dismissing the menu three times no longer stops the countdown, and tray
  Settings…, Open Nightcap and Quit (with restore, mid-fade) act immediately.
- **The dimming overlay was never drawn:** winit's mouse passthrough marks a
  window layered without setting layered attributes, and Windows does not draw
  such a window. Once attributes were applied, the fullscreen request turned
  out to be lost too: the overlay was a monitor-sized window anchored at the
  root's centre. Moving it afterwards left a caption strip over a transparent
  surface. The earlier report's overlay check confirmed the window existed and
  that clicks passed through, not that it was drawn. Windows now uses a native
  black layered popup whose window opacity is the overlay alpha. A capture with
  `CAPTUREBLT` shows the whole primary monitor and taskbar dimmed, and
  `WindowFromPoint` under it returns the Nightcap window. Real clicks on Pause
  and Cancel pass through while running, paused and finished, and timers started
  from the tray dim while the window is hidden. The popup follows display
  changes itself, because a paused timer with a hidden root gets no app updates.
- **A virtual display masked native failure:** the MSI monitor ignores writes,
  but the virtual display at 0 accepted its (unchanging) writes, so every
  failure was reported as partial. The engine kept "fading natively" and never
  showed the overlay, so nothing visibly dimmed. Displays already at their
  minimum are now left out of each fade's capture; the hardware test reports
  overlay alpha 0.200. The startup check still counts them as supported, so a
  display raised from its minimum later fades natively.
- **Second instances:** a second launch started another engine and tray icon.
  It captured the first instance's faded levels as its originals, and both
  timers wrote to the same devices. The running copy now owns a session-local
  named mutex until it has restored levels and exited. A later launch finds the
  running copy's message-only listener window and asks it, with
  `SendMessageTimeout`, to show its window. That call's reply answers only that
  launch, so no answer can be left over for another one. The UI answers after
  handling that pass's Quit, so a copy that is quitting refuses. Accepting means
  the show and focus commands were issued; focus remains subject to Windows'
  foreground rules, and only the running copy's process is allowed to take the
  foreground. A request the UI reaches after its launch stopped waiting expires
  rather than opening the window later. A refused launch, or one made after a
  crash (which abandons the mutex), keeps waiting: it is shown by whichever launch
  took over, or starts normally once it owns the mutex itself. A launch
  that was shown exits with code 0. One that cannot open the running copy (for
  example because it runs elevated), or gets no answer within 10 seconds, exits
  with code 1 instead of starting a second engine. Live: two simultaneous
  launches were both answered within 500 ms with Nightcap in the foreground, and
  a launch fired immediately after tray Quit started its own window.
- **Left-clicking the tray icon:** tray-icon opens the menu on left clicks by
  default. That modal menu swallowed the double click that was meant to restore
  the window. A left click now opens the window and the menu stays on the right
  button.
- **Settings written with a BOM:** PowerShell 5's UTF-8 output (and older
  Notepad) starts with a byte-order mark. serde_json rejected it, so every
  preference silently reverted to defaults and the file was then overwritten.
  The loader now accepts a leading BOM.
- **Per-frame window icon:** the Settings viewport rebuilt the 128×128
  supersampled icon on every root pass. egui compares icons by pointer, so it
  also re-sent `SetWindowIcon` every frame. The icon is now built once.

## Executed verification

```powershell
cargo fmt --check
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo build --release --locked
cargo test --locked --test windows_runtime -- --ignored --nocapture --test-threads=1
```

- Default suite: 106 library tests, 1 binary test and 2 Windows runtime tests
  passed; the hardware test is opt-in. The overlay viewport tests now run on
  macOS and Linux only; Windows tests the native dimmer, with a real window.
- Opt-in hardware suite passed: volume 0.344 → 0.257 and restored; overlay
  alpha 0.200 after the monitor ignored DDC writes.
- Release binary: idle CPU 0 s over 5 s with the window visible; a second
  launch exited with code 0 and showed the first; tray Quit exited.

## Remaining limits

- The notification-area menu is drawn in the light theme while Windows is dark.
  muda offers dark themes only for menu bars; dark popup menus need the
  undocumented uxtheme `SetPreferredAppMode`, so this was left alone.
- When some displays fade natively and others fail, the failed displays are not
  dimmed. The overlay covers only the root window's monitor, and only when
  native fading fails everywhere.
- Linux has no single-instance guard; macOS relies on LaunchServices for the
  app bundle.
- A display is treated as unable to dim when it sits at the bottom of its
  reported range (DDC levels are normalized to the monitor's own range). A WMI
  panel whose lowest supported level is above 0%, already at that level, still
  counts as fading natively. This host has no WMI panel, so that case was not
  reproduced. When every detected display is excluded, the overlay dims instead,
  and the window reports both the missing brightness control and the reason.
- 32-bit Windows builds were not compiled; releases are x64 only.
- Close-to-tray trusts that the tray icon was created. If Explorer has not yet
  accepted it (tray-icon retries when the taskbar restarts), relaunching
  Nightcap now shows the hidden window.
- Actual sleep, hibernate, shutdown and wake were still not exercised.
