# Linux verification run — 2026-10-19

Nightcap builds and its engine and tray commands work on this host. Desktop
verification remains incomplete: the native X11 run reproduced blocked pointer
input under the overlay and delayed Settings propagation. Those issues remain
unfixed; passing engine tests do not establish that the desktop experience works.

## Environment

- Ubuntu 26.04.1 LTS, x86_64, kernel 7.0.0-38-generic; Rust 1.99.1.
- The agent account has no access to the logged-in user's graphical session or
  GPU devices. Native GUI tests used a separate Xvfb display, Openbox 3.6.1,
  xcompmgr, and Mesa llvmpipe software Vulkan rendering. These checks exercised
  the compiled egui app, not the HTML design mockup.
- No `/sys/class/backlight` devices were present. Hardware brightness could not
  be tested; dimming used the overlay fallback.
- PipeWire 1.6.2 exposed only **Dummy Output** to this account. Volume API writes
  and read-back were tested, but no physical audio device or audible output was
  tested. The initial level was 1.00 and was restored to 1.00 after testing.
- logind's `CanHibernate` returned `s "na"`; Hibernate was unavailable.
- The account's session bus had no StatusNotifier host. Tray protocol tests used
  a temporary Gio D-Bus StatusNotifierWatcher advertising a registered host.
  This tests Nightcap's native service/menu dispatch, not a visible GNOME/KDE tray.

Rust was installed for the agent account. X11 tools and extra library packages
were downloaded from Ubuntu and extracted locally without privileged package
installation. `XDG_CONFIG_HOME` isolated test preferences from existing settings.
Every completion used `NIGHTCAP_DRY_RUN=1` or recorded `DryRunPower`; no actual
suspend, hibernation, or shutdown was requested.

## Executed checks

```sh
cargo test --locked --all-targets
cargo test --locked --test linux_runtime -- --ignored --nocapture --test-threads=1
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo build --release --locked
```

- Default suite: **81 passed, zero failed**, one native test intentionally ignored.
  Other platforms' native tests are compiled out on Linux.
- Opt-in Linux suite: **one passed**. `tests/linux_runtime.rs` adds read-only native
  discovery and transfer to a worker thread, plus an ignored test that changes
  available native levels with an independent restoration guard.
- The opt-in test verified volume falling from 1.00 to 0.75, logical overlay
  opacity 0.225, pause, extension, resume, cancellation, dry-run Sleep/Shutdown
  dispatch, synthetic wake, restoration disabled, and engine shutdown.
  Restoration read back 1.00. Hibernate completion was skipped as unavailable.
  This test drives the engine directly with a controlled clock; separate GUI/tray
  checks exercise its background worker and accelerated real clock.
- Formatting and Clippy passed. Initial Clippy failed on Linux tray icon packing
  (`chunks_exact_to_as_chunks`); using `as_chunks::<4>().0.iter()` fixed the lint
  without changing pixel order.
- The optimized build passed. The executable, README and both licenses were
  packaged using the release workflow's tarball layout, extracted, and the
  extracted executable launched successfully. Its directly linked libraries
  resolved. Local package: `target/linux-qa/Nightcap-Linux-x86_64.tar.gz`.

## Review follow-up

Claude's review identified three issues in the initial opt-in test. The restore
guard now captures its own backend once; assertion reads use a separate observer
so partial or zero readings cannot replace the guard's per-device baselines.
A deterministic regression checks cleanup after both partial and zero fades.
Temporarily restoring the old recapture behavior made that regression fail with
the second display left at 0.15 instead of its original 0.2.

Brightness assertions now allow half a native step, derived from sysfs
`max_brightness`, in addition to percentage rounding. ALSA assertions allow a 5%
mapped-mixer tolerance; PipeWire/PulseAudio retain 1.5%. A fixture with 15 native
brightness steps verifies that a 50% request reading back as 8/15 is accepted
while a 75% reading is rejected.

Before creating the restore guard or writing levels, the test checks raw
PipeWire/PulseAudio volume and refuses amplification above 100%, invalid levels,
or missing raw readings. This avoids restoring an amplified output to the
backend's clamped 100%. Regression checks cover amplification in either channel,
negative/non-finite values, and empty readings. Linux CI still runs only in the
release workflow; a Linux pull-request job is outside this change.

## Native desktop checks

- With dimming disabled, the main window accepted duration selection, Start,
  Pause, Resume and Cancel. The paused view displayed Resume; Cancel returned to
  Idle and restored volume. Settings opened in a separate native window and its
  dimming toggle changed visually.
- Without a StatusNotifier host, startup reported that closing the window would
  quit. Closing the main window exited the packaged app without leaving a hidden
  background process.
- With the test StatusNotifier host, native D-Bus menu commands exercised Start,
  Pause, an unchanged paused countdown, +15 Minutes, Resume and Cancel. Closing
  the main window hid it. A new 15-minute timer completed while hidden at
  `NIGHTCAP_SPEED=60`, logging a dry-run Sleep. Open Nightcap restored the window;
  Settings and Quit commands worked. Volume was restored after Quit.
- The extracted optimized executable reproduced the overlay failure below.
  Tray Cancel cleared the overlay, restored volume and allowed graceful quit.

## Unresolved findings

### Overlay blocks pointer input in Openbox

With default dimming enabled, start a 15-minute timer using:

```sh
NIGHTCAP_DRY_RUN=1 NIGHTCAP_SPEED=60 target/release/nightcap
```

Once the fullscreen overlay appears, pointer Pause/Resume and Cancel clicks do
not reach the main window. This reproduced in the debug executable and extracted
optimized package. In the package check, pausing through the native tray and
clicking the main window's Resume left the tray state Paused.

Native XShape inspection found zero input rectangles on the overlay client but
one on its Openbox parent. This supports the explanation that the window manager's
parent intercepts input despite the client's mouse-passthrough setting. Keyboard
Ctrl+Q and D-Bus tray commands still worked and restored levels.

The default WGPU renderer also logged that the active surface did not support a
transparent composite alpha mode. The screen visibly dimmed, but correct overlay
opacity and composition were not established. These results apply to this isolated
Openbox/software-rendering setup; GNOME Wayland and hardware GPU behavior remain
unverified.

### Settings changes wait for a main-window event

While Idle, open Settings and toggle dimming from off to on. The toggle changed
visually, but `settings.json` still contained `dim.enabled: false` after more than
five seconds, and the main window still showed Off. Moving the pointer into the
main window caused the change to propagate and save with `dim.enabled: true`.

Reliable immediate propagation and persistence of independent Settings changes
while the main window receives no events were therefore not verified.

## Remaining coverage

Actual power actions, OS wake delivery, physical backlight/audio devices,
disconnect/reconnect, output switching, multiple monitors, hardware rendering,
Wayland, native GNOME/KDE tray interaction and native light appearance still need
desktop/hardware testing. Screenshots in this run used dark appearance. The Linux
audio backend resolves the default sink for each command; this run kept it fixed
and does not establish restoration to the original sink after output switching.

Logs and screenshots are under `target/linux-qa/` (ignored by Git).

To repeat the opt-in native test, close other Nightcap instances, keep displays
and the selected audio output stable, and run the ignored-test command above
alone. It changes real brightness/volume when available and attempts to restore
captured levels even when an assertion fails. Power is always dry-run.
