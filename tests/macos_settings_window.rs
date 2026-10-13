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
        let mut opening = opening;
        let mut output = ctx.run_ui(Default::default(), |ui| {
            settings_window::show(
                ui.ctx(),
                &mut open,
                &mut opening,
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
        !settings
            .collectionBehavior()
            .contains(NSWindowCollectionBehavior::MoveToActiveSpace),
        "Settings must target Nightcap's Space rather than moving to the active Space"
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

    settings.orderFront(None);
    show_settings(false);
    assert!(
        settings.parentWindow().is_none(),
        "Settings must detach once it has been shown"
    );
    let independently_placed = settings.frame();
    main.setFrameOrigin(NSPoint::new(
        visible.origin.x + 300.0,
        visible.origin.y + 200.0,
    ));
    assert_eq!(
        settings.frame(),
        independently_placed,
        "moving Nightcap must not move an already open Settings window"
    );

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
    assert!(
        settings.parentWindow().is_none(),
        "the window must be detached before reopening"
    );
    show_settings(true);
    settings.orderFront(None);
    show_settings(false);
    assert!(
        settings.parentWindow().is_none(),
        "reopening must release the parent before interaction"
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
    settings.setFrameOrigin(NSPoint::new(
        visible.origin.x + 100.0,
        visible.origin.y + 100.0,
    ));
    // This host caches screen() even for ordered-out/offscreen NSWindows. Override
    // only that nullable getter to exercise the absent-screen condition while
    // leaving native placement and parent/child side effects real.
    objc2::define_class!(
        #[unsafe(super = NSWindow)]
        #[thread_kind = MainThreadOnly]
        struct NoScreenWindow;
        impl NoScreenWindow {
            #[unsafe(method(screen))]
            fn unavailable_screen(&self) -> *mut NSScreen { std::ptr::null_mut() }
        }
    );
    main.setTitle(&NSString::from_str("Nightcap on saved display"));
    // SAFETY: initializing a native NSWindow subclass on the main thread, with
    // the superclass initializer's exact signature and explicit retained lifetime.
    let no_screen: Retained<NoScreenWindow> = unsafe {
        objc2::msg_send![
            NoScreenWindow::alloc(mtm),
            initWithContentRect: NSRect::new(NSPoint::new(visible.origin.x + 200.0, visible.origin.y + 200.0), NSSize::new(340.0, 560.0)),
            styleMask: NSWindowStyleMask::Titled,
            backing: NSBackingStoreType::Buffered,
            defer: false
        ]
    };
    unsafe { no_screen.setReleasedWhenClosed(false) };
    no_screen.setTitle(&NSString::from_str("Nightcap"));
    assert!(
        no_screen.screen().is_none(),
        "fixture must exercise offscreen screen lookup"
    );
    show_settings(true);
    assert!(
        settings.screen().is_some(),
        "saved-frame fallback must recover a usable display"
    );
    assert_eq!(
        settings.frame().origin.x,
        visible.origin.x + 140.0,
        "saved-frame fallback must still position Settings when screen() is nil"
    );
    no_screen.setTitle(&NSString::from_str("Closed Nightcap"));
    no_screen.close();
    main.setTitle(&NSString::from_str("Nightcap"));
    settings.close();
    settings.setTitle(&NSString::from_str("Closed Settings"));
    main.setFrameOrigin(NSPoint::new(
        visible.origin.x + 200.0,
        visible.origin.y + 200.0,
    ));

    // Exercise the application's immediate-renderer boundary. Mirror winit's
    // create/show order using real AppKit windows, without a graphics device.
    use std::{cell::RefCell, rc::Rc};
    let created: Rc<RefCell<Option<Retained<NSWindow>>>> = Rc::default();
    let visibility = Rc::new(RefCell::new(Vec::new()));
    let renderer_window = Rc::clone(&created);
    let renderer_visibility = Rc::clone(&visibility);
    let root = main.clone();
    let root_frame = main.frame();
    eframe::egui::Context::set_immediate_viewport_renderer(move |ctx, mut viewport| {
        let visible = viewport.builder.visible.unwrap_or(true);
        renderer_visibility.borrow_mut().push(visible);
        if renderer_window.borrow().is_none() {
            assert!(
                !visible,
                "the first native Settings creation must be hidden"
            );
            *renderer_window.borrow_mut() = Some(window(
                mtm,
                "Settings",
                NSRect::new(visible_frame_origin(), NSSize::new(460.0, 700.0)),
            ));
        }
        let settings = renderer_window.borrow();
        let settings = settings.as_ref().unwrap();
        if visible {
            assert_eq!(
                settings.parentWindow().as_deref(),
                Some(&*root),
                "Space preparation must precede showing Settings"
            );
            assert!(
                !settings
                    .collectionBehavior()
                    .contains(NSWindowCollectionBehavior::MoveToActiveSpace)
            );
            settings.orderFront(None);
        } else {
            settings.orderOut(None);
        }
        let mut input = eframe::egui::RawInput {
            viewport_id: viewport.ids.this,
            ..Default::default()
        };
        input.viewports.insert(
            viewport.ids.this,
            eframe::egui::ViewportInfo {
                parent: Some(viewport.ids.parent),
                ..Default::default()
            },
        );
        let mut output = ctx.run_ui(input, |ui| (viewport.viewport_ui_cb)(ui));
        output.textures_delta.clear();
    });
    fn visible_frame_origin() -> NSPoint {
        NSScreen::screens(MainThreadMarker::new().unwrap())
            .firstObject()
            .unwrap()
            .visibleFrame()
            .origin
    }
    let ctx = eframe::egui::Context::default();
    ctx.set_embed_viewports(false);
    let mut opening = true;
    let mut open = true;
    let mut preferences = Settings::default();
    let mut render = |opening: &mut bool| {
        let mut output = ctx.run_ui(Default::default(), |ui| {
            settings_window::show(
                ui.ctx(),
                &mut open,
                opening,
                &mut preferences,
                false,
                false,
                false,
            );
        });
        output.textures_delta.clear();
    };
    render(&mut opening);
    assert!(
        opening,
        "native creation must not consume the pending opening request"
    );
    render(&mut opening);
    assert!(!opening);
    assert_eq!(*visibility.borrow(), vec![false, true]);
    assert!(created.borrow().as_ref().unwrap().parentWindow().is_none());
    assert_eq!(
        main.frame(),
        root_frame,
        "opening Settings must preserve Nightcap's placement"
    );

    main.setTitle(&NSString::from_str("Unavailable Nightcap"));
    opening = true;
    render(&mut opening);
    assert!(opening, "failed preparation must remain pending for retry");
    main.setTitle(&NSString::from_str("Nightcap"));
    render(&mut opening);
    assert!(!opening);
    assert_eq!(*visibility.borrow(), vec![false, true, false, true]);
    created.borrow().as_ref().unwrap().close();
    unrelated.close();
    main.close();
    println!(
        "Settings Space policy, current-display placement, reopening, size preservation, and unrelated-window isolation passed ({} displays)",
        screens.len()
    );
}
