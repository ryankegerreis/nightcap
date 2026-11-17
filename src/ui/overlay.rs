//! Passive, transparent dimming viewport, independent of the root's render callback.
//!
//! The integrator must enable transparency on NativeOptions' root viewport and return
//! `[0.0; 4]` from App::clear_color (all viewports share that clear color). Paint the
//! normal windows' backgrounds explicitly. Call `show` on every root UI pass.
//! Register the hidden child on the first root UI pass, before the root can close.
//! Logic-only updates can then reveal it without revealing or focusing the root.
//!
//! Windows uses a native layered window instead of a viewport (see `dimmer`).
//! There, call `follow_root` before `sync` so the dimming covers the root's monitor.

#[cfg(not(target_os = "windows"))]
use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};

use eframe::egui::Context;
#[cfg(not(target_os = "windows"))]
use eframe::egui::{
    self, Color32, LayerId, Order, ViewportBuilder, ViewportClass, ViewportId, WindowLevel,
};

pub struct Overlay {
    #[cfg(not(target_os = "windows"))]
    alpha: Arc<AtomicU32>,
    #[cfg(not(target_os = "windows"))]
    id: ViewportId,
    #[cfg(not(target_os = "windows"))]
    registered: bool,
    #[cfg(target_os = "windows")]
    dimmer: dimmer::Dimmer,
}

impl Default for Overlay {
    fn default() -> Self {
        Self::new()
    }
}

impl Overlay {
    pub fn new() -> Self {
        Self {
            #[cfg(not(target_os = "windows"))]
            alpha: Arc::new(AtomicU32::new(0.0_f32.to_bits())),
            #[cfg(not(target_os = "windows"))]
            id: ViewportId::from_hash_of("nightcap-dimming-overlay"),
            #[cfg(not(target_os = "windows"))]
            registered: false,
            #[cfg(target_os = "windows")]
            dimmer: dimmer::Dimmer::default(),
        }
    }

    /// The dimming covers the monitor that holds this window, hidden or not.
    #[cfg(target_os = "windows")]
    pub fn follow_root(&mut self, root: &impl raw_window_handle::HasWindowHandle) {
        use raw_window_handle::RawWindowHandle;
        if let Ok(RawWindowHandle::Win32(handle)) = root.window_handle().map(|h| h.as_raw()) {
            self.dimmer.follow(handle.hwnd.get());
        }
    }

    /// Call from every root App::ui pass. Zero alpha keeps the child hidden, ready
    /// for a fade started from the tray while the root is hidden.
    /// Covers one monitor; on macOS this follows the root's monitor without entering a
    /// fullscreen Space. On Windows it covers the root's monitor. On Linux, the window
    /// manager selects the fullscreen monitor from the root's position; Wayland may
    /// choose a different output.
    pub fn show(&mut self, ctx: &Context, alpha: f32) {
        self.sync(ctx, alpha);
        #[cfg(not(target_os = "windows"))]
        self.show_viewport(ctx);
    }

    #[cfg(not(target_os = "windows"))]
    fn show_viewport(&mut self, ctx: &Context) {
        let alpha = f32::from_bits(self.alpha.load(Ordering::Relaxed));
        self.registered = true;
        #[cfg(target_os = "macos")]
        install_macos_focus_guard();
        let builder = monitor_builder(ctx)
            .with_title("Nightcap Dimming Overlay")
            .with_visible(alpha > 0.0)
            .with_decorations(false)
            .with_transparent(true)
            .with_window_level(WindowLevel::AlwaysOnTop)
            .with_mouse_passthrough(true)
            .with_taskbar(false)
            .with_active(false)
            .with_resizable(false)
            .with_has_shadow(false);
        #[cfg(target_os = "linux")]
        let builder = builder.with_window_type(egui::X11WindowType::Dock);
        let shared_alpha = Arc::clone(&self.alpha);
        ctx.show_viewport_deferred(self.id, builder, move |ui, class| {
            // Embedded viewports would dim the app itself and intercept input.
            if class == ViewportClass::EmbeddedWindow {
                return;
            }
            #[cfg(target_os = "macos")]
            install_macos_focus_guard();
            let ctx = ui.ctx();
            let alpha = f32::from_bits(shared_alpha.load(Ordering::Relaxed));
            let rect = ctx.input(|input| input.viewport_rect());
            ctx.layer_painter(LayerId::new(
                Order::Background,
                egui::Id::new("nightcap-dim"),
            ))
            .rect_filled(
                rect,
                0.0,
                Color32::from_black_alpha((alpha * 255.0).round() as u8),
            );
        });
    }

    /// Logic-only updates keep an existing child alive when the root is hidden.
    #[cfg(not(target_os = "windows"))]
    pub fn sync(&mut self, ctx: &Context, alpha: f32) {
        let alpha = normalized(alpha);
        let previous = self.alpha.swap(alpha.to_bits(), Ordering::Relaxed);
        if self.registered && previous != alpha.to_bits() {
            // Hiding is immediate even if the root has no UI pass to remove it.
            ctx.send_viewport_cmd_to(self.id, egui::ViewportCommand::Visible(alpha > 0.0));
            ctx.request_repaint_of(self.id);
        }
    }

    /// Logic-only updates keep dimming while the root is hidden.
    #[cfg(target_os = "windows")]
    pub fn sync(&mut self, _ctx: &Context, alpha: f32) {
        self.dimmer.apply(normalized(alpha));
    }
}

fn normalized(alpha: f32) -> f32 {
    if alpha.is_finite() {
        alpha.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

#[cfg(not(target_os = "windows"))]
fn monitor_builder(ctx: &Context) -> ViewportBuilder {
    let (size, root_rect) =
        ctx.input(|input| (input.viewport().monitor_size, input.viewport().outer_rect));
    let mut builder = ViewportBuilder::default();
    if let Some(size) = size {
        builder = builder.with_inner_size(size);
    }
    #[cfg(target_os = "macos")]
    {
        // AppKit screen coordinates point upward; egui/winit coordinates point down
        // from the top of the primary screen. Divide by egui's extra zoom factor.
        use objc2::MainThreadMarker;
        use objc2_app_kit::NSScreen;
        if let Some(mtm) = MainThreadMarker::new() {
            let screens = NSScreen::screens(mtm);
            if let Some(primary) = screens.firstObject() {
                let primary_frame = primary.frame();
                let top = primary_frame.origin.y + primary_frame.size.height;
                let zoom = f64::from(ctx.zoom_factor());
                let center = root_rect.map(|rect| rect.center());
                let mut selected = None;
                for screen in &screens {
                    let frame = screen.frame();
                    let origin = egui::pos2(
                        (frame.origin.x / zoom) as f32,
                        ((top - frame.origin.y - frame.size.height) / zoom) as f32,
                    );
                    let size = egui::vec2(
                        (frame.size.width / zoom) as f32,
                        (frame.size.height / zoom) as f32,
                    );
                    let rect = egui::Rect::from_min_size(origin, size);
                    if selected.is_none() || center.is_some_and(|center| rect.contains(center)) {
                        selected = Some(rect);
                    }
                    if center.is_some_and(|center| rect.contains(center)) {
                        break;
                    }
                }
                if let Some(rect) = selected {
                    return builder.with_position(rect.min).with_inner_size(rect.size());
                }
            }
        }
        builder.with_position([0.0, 0.0])
    }
    #[cfg(not(target_os = "macos"))]
    {
        if let Some(rect) = root_rect {
            builder = builder.with_position(rect.center());
        }
        builder.with_fullscreen(true)
    }
}

/// A plain black, click-through, topmost layered popup whose opacity is the
/// overlay alpha. eframe's transparent OpenGL viewport cannot do this on Windows:
/// winit's mouse passthrough leaves it layered without attributes, so it is never
/// drawn; its fullscreen request is lost once egui applies the position and size;
/// and moving it afterwards leaves a caption strip over a transparent surface.
#[cfg(target_os = "windows")]
mod dimmer {
    use std::{marker::PhantomData, sync::Once};

    use windows::{
        Win32::{
            Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM},
            Graphics::Gdi::{
                BLACK_BRUSH, GetMonitorInfoW, GetStockObject, HBRUSH, HMONITOR,
                MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromWindow,
            },
            System::LibraryLoader::GetModuleHandleW,
            UI::WindowsAndMessaging::{
                CreateWindowExW, DefWindowProcW, DestroyWindow, GWLP_USERDATA, GetWindowLongPtrW,
                GetWindowRect, HWND_TOPMOST, IsWindowVisible, LWA_ALPHA, RegisterClassExW, SW_HIDE,
                SWP_NOACTIVATE, SWP_NOZORDER, SWP_SHOWWINDOW, SetLayeredWindowAttributes,
                SetWindowLongPtrW, SetWindowPos, ShowWindow, WM_DISPLAYCHANGE, WNDCLASSEXW,
                WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
                WS_EX_TRANSPARENT, WS_POPUP,
            },
        },
        core::{PCWSTR, w},
    };

    const CLASS: PCWSTR = w!("NightcapDimmingOverlay");

    /// Handles are stored as integers. The window belongs to the UI thread that
    /// made it, so the dimmer is not `Send`.
    #[derive(Default)]
    pub(super) struct Dimmer {
        window: Option<isize>,
        root: Option<isize>,
        /// Applied opacity; zero while hidden.
        opacity: u8,
        _thread_bound: PhantomData<*const ()>,
    }

    fn monitor_rect(monitor: HMONITOR) -> Option<RECT> {
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: valid monitor handle and output structure.
        unsafe { GetMonitorInfoW(monitor, &mut info) }
            .as_bool()
            .then_some(info.rcMonitor)
    }

    unsafe extern "system" fn procedure(
        window: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        // SAFETY: Win32 calls on the window this procedure was called for.
        unsafe {
            // A paused timer with a hidden root gets no app updates, so follow
            // resolution and layout changes here, to the root's monitor (in user data).
            // The *LongPtr functions take and return i32 on 32-bit Windows.
            let root = GetWindowLongPtrW(window, GWLP_USERDATA) as isize;
            let anchor = if root != 0 { HWND(root as _) } else { window };
            if message == WM_DISPLAYCHANGE
                && IsWindowVisible(window).as_bool()
                && let Some(rect) =
                    monitor_rect(MonitorFromWindow(anchor, MONITOR_DEFAULTTONEAREST))
            {
                let _ = SetWindowPos(
                    window,
                    None,
                    rect.left,
                    rect.top,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    SWP_NOACTIVATE | SWP_NOZORDER,
                );
            }
            DefWindowProcW(window, message, wparam, lparam)
        }
    }

    fn create() -> windows::core::Result<HWND> {
        // SAFETY: plain Win32 calls; the class's black brush paints the whole client.
        unsafe {
            let instance = HINSTANCE(GetModuleHandleW(None)?.0);
            static REGISTER: Once = Once::new();
            REGISTER.call_once(|| {
                let class = WNDCLASSEXW {
                    cbSize: size_of::<WNDCLASSEXW>() as u32,
                    lpfnWndProc: Some(procedure),
                    hInstance: instance,
                    hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
                    lpszClassName: CLASS,
                    ..Default::default()
                };
                if RegisterClassExW(&class) == 0 {
                    log::warn!(
                        "Registering the dimming overlay class failed: {}",
                        windows::core::Error::from_thread()
                    );
                }
            });
            // Transparent + layered passes input through; no-activate keeps focus put.
            CreateWindowExW(
                WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_TOPMOST
                    | WS_EX_TOOLWINDOW
                    | WS_EX_NOACTIVATE,
                CLASS,
                w!("Nightcap Dimming Overlay"),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(instance),
                None,
            )
        }
    }

    impl Dimmer {
        pub(super) fn follow(&mut self, root: isize) {
            if self.root != Some(root) {
                self.root = Some(root);
                self.share_root();
            }
        }

        /// The window procedure reads the root from user data on display changes.
        fn share_root(&self) {
            if let (Some(window), Some(root)) = (self.window, self.root) {
                // SAFETY: stores an integer in this dimmer's own window.
                unsafe { SetWindowLongPtrW(HWND(window as _), GWLP_USERDATA, root as _) };
            }
        }

        /// `alpha` is already within `0.0..=1.0`. Unchanged values cost three cheap
        /// queries. Bounds are compared with the window's actual rectangle, since
        /// display changes also move it from the window procedure.
        pub(super) fn apply(&mut self, alpha: f32) {
            let opacity = (alpha * 255.0).round() as u8;
            if opacity == 0 {
                self.hide();
                return;
            }
            let Some(window) = self.window() else {
                return;
            };
            let bounds = self.monitor_bounds();
            // SAFETY: `window` is this dimmer's live window, on its creating thread.
            unsafe {
                if opacity != self.opacity
                    && let Err(error) =
                        SetLayeredWindowAttributes(window, COLORREF(0), opacity, LWA_ALPHA)
                {
                    log::warn!("Setting the dimming opacity failed: {error}");
                    return;
                }
                let mut current = RECT::default();
                let moved = GetWindowRect(window, &mut current).is_err() || bounds != Some(current);
                if (self.opacity == 0 || moved)
                    && let Some(rect) = bounds
                {
                    // Cover the root's monitor above other topmost windows, without focus.
                    if let Err(error) = SetWindowPos(
                        window,
                        Some(HWND_TOPMOST),
                        rect.left,
                        rect.top,
                        rect.right - rect.left,
                        rect.bottom - rect.top,
                        SWP_NOACTIVATE | SWP_SHOWWINDOW,
                    ) {
                        log::warn!("Showing the dimming overlay failed: {error}");
                        return;
                    }
                }
            }
            self.opacity = opacity;
        }

        fn hide(&mut self) {
            if self.opacity != 0
                && let Some(window) = self.window
            {
                // SAFETY: hides this dimmer's own live window.
                let _ = unsafe { ShowWindow(HWND(window as _), SW_HIDE) };
            }
            self.opacity = 0;
        }

        fn monitor_bounds(&self) -> Option<RECT> {
            // SAFETY: monitor query; a stale root handle falls back to the nearest monitor.
            monitor_rect(unsafe {
                match self.root {
                    Some(root) => MonitorFromWindow(HWND(root as _), MONITOR_DEFAULTTONEAREST),
                    None => MonitorFromWindow(HWND::default(), MONITOR_DEFAULTTOPRIMARY),
                }
            })
        }

        fn window(&mut self) -> Option<HWND> {
            if self.window.is_none() {
                match create() {
                    Ok(window) => {
                        self.window = Some(window.0 as isize);
                        self.share_root();
                    }
                    Err(error) => log::warn!("Creating the dimming overlay failed: {error}"),
                }
            }
            self.window.map(|window| HWND(window as _))
        }
    }

    impl Drop for Dimmer {
        fn drop(&mut self) {
            if let Some(window) = self.window.take() {
                // SAFETY: destroys this dimmer's own window, on its creating thread.
                let _ = unsafe { DestroyWindow(HWND(window as _)) };
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use windows::Win32::UI::WindowsAndMessaging::{
            GWL_EXSTYLE, GetLayeredWindowAttributes, GetWindowLongPtrW, GetWindowRect, IsWindow,
            IsWindowVisible,
        };

        use super::*;

        fn opacity(window: HWND) -> u8 {
            let mut alpha = 0;
            // SAFETY: live window; valid output pointer.
            unsafe { GetLayeredWindowAttributes(window, None, Some(&mut alpha), None) }.unwrap();
            alpha
        }

        #[test]
        fn dims_the_whole_primary_monitor_click_through_and_hides_at_zero() {
            let mut dimmer = Dimmer::default();
            dimmer.apply(0.0);
            assert!(dimmer.window.is_none(), "nothing to dim creates no window");
            dimmer.apply(0.5);
            let window = HWND(dimmer.window.unwrap() as _);
            let mut rect = RECT::default();
            // SAFETY: live window owned by this test thread; valid output pointers.
            unsafe {
                assert!(IsWindowVisible(window).as_bool());
                GetWindowRect(window, &mut rect).unwrap();
                let style = GetWindowLongPtrW(window, GWL_EXSTYLE) as u32;
                for required in [
                    WS_EX_LAYERED,
                    WS_EX_TRANSPARENT,
                    WS_EX_TOPMOST,
                    WS_EX_NOACTIVATE,
                ] {
                    assert_eq!(style & required.0, required.0, "{required:?}");
                }
            }
            assert_eq!(Some(rect), dimmer.monitor_bounds());
            assert_eq!(opacity(window), 128);
            dimmer.apply(0.9);
            assert_eq!(opacity(window), 230);
            dimmer.apply(0.0);
            // SAFETY: as above.
            assert!(!unsafe { IsWindowVisible(window) }.as_bool());
            dimmer.apply(0.2);
            // SAFETY: as above.
            assert!(unsafe { IsWindowVisible(window) }.as_bool());
            assert_eq!(opacity(window), 51);
            drop(dimmer);
            // SAFETY: only queries whether the handle still names a window.
            assert!(!unsafe { IsWindow(Some(window)) }.as_bool());
        }
    }
}

/// winit overrides key/main eligibility for every NSWindow. Preserve its original
/// behavior for normal windows while rejecting this passive overlay. Patch methods
/// rather than changing the object's class: AppKit KVO owns the window's isa pointer.
#[cfg(target_os = "macos")]
fn install_macos_focus_guard() {
    use objc2::rc::Retained;
    use objc2::{
        MainThreadMarker, msg_send,
        runtime::{AnyClass, AnyObject, Bool, Imp, Sel},
        sel,
    };
    use objc2_foundation::NSString;
    use std::sync::{Once, OnceLock};

    static INSTALL: Once = Once::new();
    static KEY: OnceLock<Imp> = OnceLock::new();
    static MAIN: OnceLock<Imp> = OnceLock::new();
    type Getter = unsafe extern "C-unwind" fn(&AnyObject, Sel) -> Bool;

    unsafe fn eligibility(window: &AnyObject, selector: Sel, original: Imp) -> Bool {
        // SAFETY: the hooks are installed only on NSWindow's BOOL eligibility getters.
        let title: Retained<NSString> = unsafe { msg_send![window, title] };
        if title.to_string() == "Nightcap Dimming Overlay" {
            return Bool::NO;
        }
        let getter: Getter = unsafe { std::mem::transmute(original) };
        unsafe { getter(window, selector) }
    }
    unsafe extern "C-unwind" fn key(window: &AnyObject, selector: Sel) -> Bool {
        unsafe { eligibility(window, selector, *KEY.get().expect("original key getter")) }
    }
    unsafe extern "C-unwind" fn main(window: &AnyObject, selector: Sel) -> Bool {
        unsafe { eligibility(window, selector, *MAIN.get().expect("original main getter")) }
    }
    if MainThreadMarker::new().is_none() {
        return;
    }
    let Some(class) = AnyClass::get(c"WinitWindow") else {
        return;
    };
    INSTALL.call_once(|| {
        let key_method = class
            .instance_method(sel!(canBecomeKeyWindow))
            .expect("winit key getter");
        let main_method = class
            .instance_method(sel!(canBecomeMainWindow))
            .expect("winit main getter");
        KEY.set(key_method.implementation())
            .expect("one focus guard");
        MAIN.set(main_method.implementation())
            .expect("one focus guard");
        // SAFETY: replacement methods have exactly the original getters' ABI and
        // signature; each delegates to its saved original for all normal windows.
        unsafe {
            key_method.set_implementation(std::mem::transmute::<Getter, Imp>(key));
            main_method.set_implementation(std::mem::transmute::<Getter, Imp>(main));
        }
    });
}

// Windows dims with a native window; its tests live in `dimmer`.
#[cfg(all(test, not(target_os = "windows")))]
mod tests {
    use super::*;

    #[test]
    fn zero_or_invalid_alpha_registers_a_hidden_passive_viewport() {
        let ctx = Context::default();
        ctx.set_embed_viewports(false);
        let mut overlay = Overlay::new();
        for alpha in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            let mut output = ctx.run_ui(Default::default(), |ui| overlay.show(ui.ctx(), alpha));
            output.textures_delta.clear();
            assert_eq!(
                output.viewport_output[&overlay.id].builder.visible,
                Some(false)
            );
        }
    }

    #[test]
    fn dimming_child_is_passive_and_hides_when_cleared() {
        let ctx = Context::default();
        ctx.set_embed_viewports(false);
        let mut overlay = Overlay::new();
        let mut output = ctx.run_ui(Default::default(), |ui| overlay.show(ui.ctx(), 0.6));
        output.textures_delta.clear();
        let child = &output.viewport_output[&overlay.id];
        assert!(child.class == ViewportClass::Deferred);
        assert_eq!(child.builder.transparent, Some(true));
        assert_eq!(child.builder.decorations, Some(false));
        assert_eq!(child.builder.active, Some(false));
        assert_eq!(child.builder.mouse_passthrough, Some(true));
        assert_eq!(child.builder.taskbar, Some(false));
        assert_eq!(child.builder.window_level, Some(WindowLevel::AlwaysOnTop));
        let mut output = ctx.run_ui(Default::default(), |ui| overlay.show(ui.ctx(), 0.0));
        output.textures_delta.clear();
        assert_eq!(
            output.viewport_output[&overlay.id].builder.visible,
            Some(false)
        );
    }
    #[test]
    fn logic_only_updates_reveal_and_hide_the_registered_child() {
        let ctx = Context::default();
        ctx.set_embed_viewports(false);
        let mut overlay = Overlay::new();
        let mut output = ctx.run_ui(Default::default(), |ui| overlay.show(ui.ctx(), 0.0));
        output.textures_delta.clear();
        for (alpha, visible) in [(0.6, true), (0.0, false)] {
            let output = ctx.run_logic(&Default::default(), |ctx| overlay.sync(ctx, alpha));
            assert!(
                output.viewport_commands[&overlay.id]
                    .contains(&egui::ViewportCommand::Visible(visible))
            );
        }
    }
}
