//! Passive, transparent dimming viewport, independent of the root's render callback.
//!
//! The integrator must enable transparency on NativeOptions' root viewport and return
//! `[0.0; 4]` from App::clear_color (all viewports share that clear color). Paint the
//! normal windows' backgrounds explicitly. Call `show` on every root UI pass.
//! An existing deferred child keeps rendering while the root is hidden; creating it
//! for the first time still requires a root UI pass, not App::logic.

use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};

use eframe::egui::{
    self, Color32, Context, LayerId, Order, ViewportBuilder, ViewportClass, ViewportId, WindowLevel,
};

pub struct Overlay {
    alpha: Arc<AtomicU32>,
    id: ViewportId,
}

impl Default for Overlay {
    fn default() -> Self {
        Self::new()
    }
}

impl Overlay {
    pub fn new() -> Self {
        Self {
            alpha: Arc::new(AtomicU32::new(0.0_f32.to_bits())),
            id: ViewportId::from_hash_of("nightcap-dimming-overlay"),
        }
    }

    /// Call from the root App::ui. Zero alpha omits (and therefore destroys) the viewport.
    /// Covers one monitor; on macOS this follows the root's monitor without entering a
    /// fullscreen Space. On other desktops, the window manager selects the fullscreen
    /// monitor from the root's position; Wayland may choose a different output.
    pub fn show(&mut self, ctx: &Context, alpha: f32) {
        let alpha = if alpha.is_finite() {
            alpha.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let previous = self.alpha.swap(alpha.to_bits(), Ordering::Relaxed);
        if alpha == 0.0 {
            return;
        }
        #[cfg(target_os = "macos")]
        install_macos_focus_guard();
        let builder = monitor_builder(ctx)
            .with_title("Nightcap Dimming Overlay")
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
            #[cfg(target_os = "windows")]
            make_windows_overlay_passive();
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
        if previous != alpha.to_bits() {
            ctx.request_repaint_of(self.id);
        }
    }
}

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

/// Keep Windows from selecting the overlay when another app window is hidden.
#[cfg(target_os = "windows")]
fn make_windows_overlay_passive() {
    use windows::Win32::{
        Foundation::{HWND, LPARAM},
        UI::WindowsAndMessaging::{
            EnumWindows, GWL_EXSTYLE, GetWindowLongPtrW, GetWindowTextW, GetWindowThreadProcessId,
            SetWindowLongPtrW, WS_EX_APPWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
        },
    };
    use windows::core::BOOL;
    unsafe extern "system" fn configure(window: HWND, _param: LPARAM) -> BOOL {
        let mut process = 0;
        let mut title = [0_u16; 64];
        // SAFETY: EnumWindows supplied a live handle; buffers and out-pointers are valid.
        unsafe {
            GetWindowThreadProcessId(window, Some(&mut process));
            if process == std::process::id() {
                let length = GetWindowTextW(window, &mut title) as usize;
                if String::from_utf16_lossy(&title[..length]) == "Nightcap Dimming Overlay" {
                    let old = GetWindowLongPtrW(window, GWL_EXSTYLE);
                    let new = (old | WS_EX_NOACTIVATE.0 as isize | WS_EX_TOOLWINDOW.0 as isize)
                        & !(WS_EX_APPWINDOW.0 as isize);
                    if old != new {
                        SetWindowLongPtrW(window, GWL_EXSTYLE, new);
                    }
                }
            }
        }
        BOOL(1)
    }
    // SAFETY: callback has the required ABI and no borrowed state escapes.
    if let Err(error) = unsafe { EnumWindows(Some(configure), LPARAM(0)) } {
        log::warn!("Configuring passive overlay window failed: {error}");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_alpha_has_no_native_viewport_and_invalid_alpha_is_ignored() {
        let ctx = Context::default();
        ctx.set_embed_viewports(false);
        let mut overlay = Overlay::new();
        for alpha in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            let mut output = ctx.run_ui(Default::default(), |ui| overlay.show(ui.ctx(), alpha));
            output.textures_delta.clear();
            assert!(!output.viewport_output.contains_key(&overlay.id));
        }
    }

    #[test]
    fn dimming_child_is_passive_and_disappears_when_cleared() {
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
        assert!(!output.viewport_output.contains_key(&overlay.id));
    }
}
