# macOS hardening run — 2026-10-11

Test host: Apple M3 Pro MacBook Pro, macOS 27.2 (26B5091g), one built-in
Retina display, built-in speakers, and a Steam virtual audio output. Native UI
runs used the ad-hoc signed app bundle. Every timer end action used
`NIGHTCAP_DRY_RUN=1` or `DryRunPower`; no real sleep or shutdown was requested.

## Changes and evidence

### Keep fades and restoration on the captured device

CoreAudio previously resolved the default output on every write. After capturing
25% on the built-in speakers and switching to the virtual output, cancellation
restoration changed that output from 12.5% to 25%. The opt-in regression failed
before the fix and passed afterward. Native writes now address the captured
output; a later fade captures the then-current default output. The AppleScript
fallback cannot address an output directly, so it rejects a write when the
selected output differs from the captured one. Existing engine restoration retries
can restore it once that output is selected again.

Brightness had the same routing problem: every write selected the current primary
display. DisplayServices and the Intel IODisplay fallback now retain the captured
display/service. A simulated primary-display change failed before the fix and
passed afterward; a subsequent capture correctly selects the new display.
Physical multi-monitor switching was not executed on this one-display host.

### Expose custom controls to macOS accessibility

The native main-window tree previously contained unnamed `unknown` controls
instead of Start, Cancel, Settings, duration choices, and fade switches. Custom
widgets now supply names, roles, selected/toggled values, and disabled state.
Countdown text and minute-stepper buttons are also exposed. The native tree
confirmed the updated controls, and accessible button actions started and
canceled a timer.

Target sliders now support pointer-to-keyboard focus, left/right arrows in 5%
steps, and accessibility SetValue/Increment/Decrement actions. Requests are
clamped to the slider range, and non-finite requests are ignored. Regression
tests failed before the corresponding fixes. A native Settings click followed
by Right changed the target from 55% to 60%.

This does not resolve the separate Settings window's native accessibility bridge;
see the remaining limitations below.

### Keep settings-group animations independent

Sibling egui UIs shared a stable ID. Row-opacity animations keyed from that ID
also shared state, so disabling screen dimming made unrelated volume and restore
rows appear disabled. A headless regression observed an enabled group's opacity
at 0.4667 instead of 1.0. Each group now has its own explicit ID salt. The test
passes, and native Settings screenshots confirmed that volume, restore, and
menu-bar rows stay fully visible when screen dimming is off.

### Clean temporary bundle assets on success and failure

The macOS bundle script left both its iconset directory and DMG staging app
behind. An instrumented `mktemp` run recorded two retained build directories.
The script now uses one temporary directory and removes it on exit, including
errors and handled interruption. Re-running a full DMG build left no staging
directories. An injected icon-generation failure exited with status 42 and also
left none. The app's code signature and DMG checksum verified successfully.

## Feature coverage

| Feature | Executed verification |
| --- | --- |
| Presets and custom durations | Native 15-minute and custom 1-minute runs; duration bounds and same-frame Start/Enter commit tests; tray preset dispatch tests |
| Countdown, pause, resume, extension | Native start/pause/resume; deterministic timing tests; real hardware pause and +15-minute extension through engine commands |
| Screen and volume fades | Real DisplayServices/CoreAudio writes; independent fade scheduling, disabled fades, zero targets, errors, and downward-only behavior in automated tests |
| Cancel, wake, quit restoration | Real levels checked after Cancel, synthetic SystemWoke, and engine Shutdown; native Cmd-Q during finishing; restore-off behavior and restoration retry tests |
| Sleep and Shutdown end actions | Native picker and accelerated finishing UI; log-confirmed dry-run dispatch for both actions; power-error regression tests |
| Hibernate availability | Hidden in the native macOS picker; saved/unavailable selection normalization and menu tests |
| Settings and persistence | Native Cmd-comma/Cmd-W reopen; timing picker constraints, target pointer/keyboard editing, fade toggle and menu-title preference; JSON round-trip, corruption and bounds tests |
| Close-to-menu-bar and reopen | Native Cmd-W hid the root while the timer stayed paused/running; Cmd-comma reopened Settings and root; close-event regression tests |
| Tray controls and title | Native tray initialized; callback dispatch, queue, display text and state tests; no full native status-menu click pass |
| Overlay fallback | Temporary QA build forced brightness unavailable; a real 1512×982 overlay remained on-screen while the root was hidden; Settings/root remained usable; cancel/quit cleared it; passive viewport and hide/show tests |
| Appearance | Native dark appearance; forced light appearance in the temporary overlay QA build without changing system appearance |
| Packaging | Release .app and DMG built; `codesign --verify --deep --strict` and `hdiutil verify` passed; temporary assets cleaned on success and failure |

The temporary overlay QA source was removed after building. It changed only the
backend selection and theme in a throwaway example; production startup retains
normal capability detection and the system theme.

## Repeatable checks

Routine checks passed: 71 library tests, zero failures, strict Clippy, formatting,
and compile checks for Apple Silicon, Intel macOS, Windows x64 and Linux x64.
Three real-hardware tests remain opt-in and were run separately on this host:

```sh
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
cargo fmt --check
cargo check --locked --target x86_64-apple-darwin
cargo check --locked --target x86_64-pc-windows-msvc
cargo check --locked --target x86_64-unknown-linux-gnu

# Quit Nightcap first; these tests affect real hardware and restore it afterward.
cargo test platform_probe -- --ignored --nocapture --test-threads=1
cargo test --test macos_runtime -- --ignored --nocapture --test-threads=1
# Supply an existing, different output with software volume; IDs are host-specific.
NIGHTCAP_QA_ALTERNATE_OUTPUT=<device-id> cargo test output_switch_does_not_redirect -- --ignored --nocapture --test-threads=1
```

The native hardware test uses nonzero starting levels, targets half the captured
levels, and always substitutes DryRunPower. Hardware controls retained by a cleanup
guard restore the initial levels if an assertion fails. The output-switch test
also restores the selected output and each captured volume.

Final hardware readback matched the starting brightness **0.622544** and volume
**0.250000**. The original settings file was restored byte-for-byte, and the QA
app was quit.

## Remaining limitations and improvements

- **Separate-window accessibility:** native Settings still exposes its window
  chrome without a content tree. In eframe 0.36.2, the native Glow and WGPU
  integrations initialize AccessKit only for the root viewport. Settings widget
  semantics are present, but a child-window bridge or a different accessible
  Settings presentation needs separate work. A full VoiceOver user-flow test
  was not performed.
- **Real power transitions:** actual sleep, shutdown, Automation permission
  prompts, and native wake-notification delivery remain unverified. Restoration
  was tested with the real hardware backends and an explicit wake command.
- **Other hardware and operating systems:** Bluetooth/HDMI/USB outputs, unplug and
  reconnect, physical multiple displays, Intel runtime, and older macOS releases
  require those systems. Compilation and simulated device changes do not prove
  their runtime behavior.
- **Native status-menu interaction:** automation could inspect the app windows
  but could not attach to the macOS status menu. Its actions were covered at the
  dispatch/engine boundary; real menu clicking, menu-bar title rendering, and
  Finder/Dock reopen were not independently verified.
- **Distribution:** ad-hoc signing remains unchanged. Developer ID signing and
  notarization would improve first-install behavior but require a signing identity.
  `--install` was not executed during this run.
