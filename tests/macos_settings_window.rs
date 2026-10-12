//! Opt-in AppKit window QA, on the process main thread. No timer or hardware controls.
//! Run: cargo test --locked --test macos_settings_window -- --ignored

#[cfg(not(target_os = "macos"))]
fn main() {}

#[cfg(target_os = "macos")]
fn main() {
    if !std::env::args().any(|arg| arg == "--ignored") {
        println!("macos_settings_window ignored: requires a macOS window-server session");
        return;
    }
    use nightcap::{settings::Settings, ui::settings_window};
    use objc2::{MainThreadMarker, MainThreadOnly, rc::Retained};
    use objc2_app_kit::{
        NSApplication, NSBackingStoreType, NSScreen, NSWindow, NSWindowCollectionBehavior,
        NSWindowStyleMask,
    };
    use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

    fn show_settings(opening: bool) {
        let ctx = eframe::egui::Context::default();
        let mut open = true;
        let mut settings = Settings::default();
        let mut output = ctx.run_ui(Default::default(), |ui| {
            settings_window::show(
                ui.ctx(),
                &mut open,
                opening,
                &mut settings,
                false,
                false,
                false,
            );
        });
        output.textures_delta.clear();
    }

    fn window(mtm: MainThreadMarker, title: &str, rect: NSRect) -> Retained<NSWindow> {
        // SAFETY: AppKit is initialized on the main thread; these windows are
        // retained explicitly and never release themselves on close.
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                rect,
                NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&NSString::from_str(title));
        window
    }

    let mtm = MainThreadMarker::new().expect("AppKit QA must run on the process main thread");
    let _app = NSApplication::sharedApplication(mtm);
    let screens = NSScreen::screens(mtm);
    let primary = screens.firstObject().expect("requires a display");
    let visible = primary.visibleFrame();
    let main = window(
        mtm,
        "Nightcap",
        NSRect::new(
            NSPoint::new(visible.origin.x + 200.0, visible.origin.y + 200.0),
            NSSize::new(340.0, 560.0),
        ),
    );
    let settings = window(
        mtm,
        "Settings",
        NSRect::new(visible.origin, NSSize::new(460.0, 700.0)),
    );
    let unrelated = window(
        mtm,
        "Nightcap Dimming Overlay",
        NSRect::new(visible.origin, NSSize::new(100.0, 100.0)),
    );
    let unrelated_frame = unrelated.frame();
    let unrelated_behavior = unrelated.collectionBehavior();
    let size = settings.frame().size;

    show_settings(true);
    assert!(
        settings
            .collectionBehavior()
            .contains(NSWindowCollectionBehavior::MoveToActiveSpace),
        "focusing Settings must bring it to the active Space"
    );
    assert!(
        !settings.isVisible(),
        "preparing Settings must not show or focus it"
    );
    assert_eq!(settings.parentWindow().as_deref(), Some(&*main));
    assert_eq!(
        settings.frame().size,
        size,
        "opening must preserve the user's window size"
    );
    assert_ne!(
        settings.frame().origin,
        visible.origin,
        "Settings must follow the main window"
    );
    assert_eq!(unrelated.frame(), unrelated_frame);
    assert_eq!(unrelated.collectionBehavior(), unrelated_behavior);

    settings.setFrameOrigin(NSPoint::new(
        visible.origin.x + 50.0,
        visible.origin.y + 50.0,
    ));
    let user_position = settings.frame().origin;
    show_settings(false);
    assert_eq!(
        settings.frame().origin,
        user_position,
        "ordinary frames must preserve the user's placement"
    );
    settings.orderOut(None);
    show_settings(true);
    assert_eq!(
        settings.parentWindow().as_deref(),
        Some(&*main),
        "reopening must reattach Settings after orderOut detaches it"
    );

    // Reopening must use the main window's current display, including a display
    // whose origin is negative or above the primary display.
    for screen in &screens {
        let area = screen.visibleFrame();
        main.setFrameOrigin(NSPoint::new(area.origin.x + 30.0, area.origin.y + 30.0));
        show_settings(true);
        let frame = settings.frame();
        assert!(frame.origin.x >= area.origin.x);
        if frame.size.width <= area.size.width {
            assert!(frame.origin.x + frame.size.width <= area.origin.x + area.size.width + 1.0);
        }
        if frame.size.height <= area.size.height {
            assert!(frame.origin.y >= area.origin.y);
        }
        assert!(frame.origin.y + frame.size.height <= area.origin.y + area.size.height + 1.0);
        assert_eq!(settings.screen(), main.screen());
    }
    let area = main.screen().unwrap().visibleFrame();
    settings.setContentSize(NSSize::new(size.width, area.size.height + 200.0));
    let oversized = settings.frame().size;
    show_settings(true);
    assert_eq!(settings.frame().size, oversized);
    let frame = settings.frame();
    assert!(
        (frame.origin.y + frame.size.height - area.origin.y - area.size.height).abs() < 1.0,
        "an oversized window's title bar must remain reachable"
    );
    settings.close();
    settings.setTitle(&NSString::from_str("Closed Settings"));
    show_settings(true); // A missing native window must be harmless.
    unrelated.close();
    main.close();
    println!(
        "Settings Space policy, current-display placement, reopening, size preservation, and unrelated-window isolation passed ({} displays)",
        screens.len()
    );
}
