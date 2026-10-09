# Nightcap

A sleep timer for your computer, like the one on a TV. Set a time, and Nightcap slowly dims the screen and lowers the volume, then puts the computer to sleep.

Nightcap is built in Rust with [egui](https://github.com/emilk/egui) and runs on macOS, Windows and Linux. It works offline and needs no account.

> **Status:** early development. The app opens a window and nothing more yet. The features below are planned.

![Nightcap main window in light and dark appearance](docs/design/main-window.png)

## Planned features

- **Sleep timer:** pick 15, 30, 45, 60 or 90 minutes, or set your own length.
- **Screen fade:** dim the screen to a level you choose, starting when you choose, over as long as you choose.
- **Volume fade:** lower the volume the same way, set up separately from the screen fade.
- **When the timer ends:** sleep, hibernate or shut down.
- **Restore:** put brightness and volume back when you cancel the timer or wake the computer.
- **Menu bar and system tray:** start, pause and cancel timers without opening the window.
- **Overlay fallback:** on displays that don't support brightness control, dim the screen with an overlay instead.

## Design

The interface follows macOS conventions in light and dark appearance. The mockup in [`docs/design/mockup.html`](docs/design/mockup.html) is interactive: open it in a browser to try the main window, Settings and the menu bar menu.

## Building

You need a recent stable [Rust toolchain](https://rustup.rs).

```sh
cargo run
```

On Linux, egui needs a few system libraries. See the [eframe documentation](https://github.com/emilk/egui/tree/main/crates/eframe) for the list.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution you intentionally submit for inclusion in this work, as defined in the Apache-2.0 license, is dual licensed as above, without any additional terms or conditions.
