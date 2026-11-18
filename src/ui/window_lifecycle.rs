//! When Nightcap's windows open, hide and close, and when the app quits.
//!
//! Each decision changes this module's state and returns the `WindowRequest`s that
//! carry it out, in order. `perform` is the only code that touches the real windows.

use std::{sync::mpsc, time::Instant};

use eframe::egui;

use crate::engine::{Command, EngineHandle};

use super::settings_window;

/// Frames to wait before focusing Settings. The immediate child's creation frame can
/// still carry root window commands, so focus once those have drained.
const SETTINGS_FOCUS_FRAMES: u8 = 2;

/// A later launch's request to show the window: its reply, and when it expires.
pub type Activation = (mpsc::Sender<bool>, Instant);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WindowRequest {
    ShowRoot { focus: bool },
    HideRoot { and_settings: bool },
    CloseRoot,
    FocusSettings,
    RepaintRoot,
    StopEngine,
}

pub(super) struct WindowLifecycle {
    /// Without a tray nothing could reopen a hidden window, so closing quits.
    has_tray: bool,
    root_visible: bool,
    quitting: bool,
    close_handled: bool,
    settings_open: bool,
    settings_opening: bool,
    settings_focus_frames: u8,
}

impl WindowLifecycle {
    pub(super) fn new(has_tray: bool) -> Self {
        Self {
            has_tray,
            root_visible: true,
            quitting: false,
            close_handled: false,
            settings_open: false,
            settings_opening: false,
            settings_focus_frames: 0,
        }
    }

    pub(super) fn root_visible(&self) -> bool {
        self.root_visible
    }

    pub(super) fn show_root(&mut self, focus: bool) -> Vec<WindowRequest> {
        self.root_visible = true;
        vec![WindowRequest::ShowRoot { focus }]
    }

    pub(super) fn open_settings(&mut self) -> Vec<WindowRequest> {
        self.settings_open = true;
        self.settings_opening = true;
        self.settings_focus_frames = SETTINGS_FOCUS_FRAMES;
        vec![WindowRequest::RepaintRoot]
    }

    /// Settings needs the root, so a hidden one returns without taking focus.
    pub(super) fn open_settings_from_tray(&mut self) -> Vec<WindowRequest> {
        let mut requests = if self.root_visible {
            Vec::new()
        } else {
            self.show_root(false)
        };
        requests.extend(self.open_settings());
        requests
    }

    pub(super) fn quit(&mut self) -> Vec<WindowRequest> {
        let mut requests = Vec::new();
        if !self.quitting {
            self.quitting = true;
            requests.push(WindowRequest::StopEngine);
        }
        requests.push(WindowRequest::CloseRoot);
        requests
    }

    /// Handles the native close button and the close shortcut. Logic-only passes retain
    /// pending input until the root renders again, so each native close counts once and
    /// its stale event cannot hide a tray reopen. Hidden input retains the last UI's key
    /// events, so `shortcut` is only consumed while the root is visible.
    pub(super) fn close(
        &mut self,
        native_close: bool,
        shortcut: impl FnOnce() -> bool,
    ) -> Vec<WindowRequest> {
        let new_close = native_close && !self.close_handled;
        self.close_handled = native_close;
        if !new_close && !(self.root_visible && shortcut()) {
            return Vec::new();
        }
        if self.has_tray && !self.quitting {
            self.hide_root()
        } else {
            self.quit()
        }
    }

    fn hide_root(&mut self) -> Vec<WindowRequest> {
        self.root_visible = false;
        let and_settings = std::mem::take(&mut self.settings_open);
        vec![WindowRequest::HideRoot { and_settings }]
    }

    /// Answers every pending, unexpired launch request, calling `show` first unless
    /// quitting. A refused launch starts its own copy once this one has exited.
    /// Expired requests are dropped: their launch has already given up waiting.
    pub(super) fn answer_activations(
        &mut self,
        activations: &mpsc::Receiver<Activation>,
        now: Instant,
        show: impl FnOnce(Vec<WindowRequest>),
    ) {
        let pending: Vec<_> = activations
            .try_iter()
            .filter(|&(_, expires)| now <= expires)
            .map(|(reply, _)| reply)
            .collect();
        if pending.is_empty() {
            return;
        }
        let accepted = !self.quitting;
        if accepted {
            show(self.show_root(true));
        }
        for reply in pending {
            let _ = reply.send(accepted);
        }
    }

    /// The `open` and `opening` flags that `settings_window::show` updates.
    pub(super) fn settings_flags(&mut self) -> (&mut bool, &mut bool) {
        (&mut self.settings_open, &mut self.settings_opening)
    }

    /// Call after each frame that showed Settings, to focus it once it has opened.
    /// Frames with the root hidden do not count toward the wait.
    pub(super) fn settings_frame_shown(&mut self) -> Vec<WindowRequest> {
        if !self.root_visible
            || self.settings_focus_frames == 0
            || !self.settings_open
            || self.settings_opening
        {
            return Vec::new();
        }
        self.settings_focus_frames -= 1;
        let mut requests = Vec::new();
        if self.settings_focus_frames == 0 {
            requests.push(WindowRequest::FocusSettings);
        }
        requests.push(WindowRequest::RepaintRoot);
        requests
    }
}

/// Carries out one request on the real windows.
pub(super) fn perform(ctx: &egui::Context, engine: &EngineHandle, request: WindowRequest) {
    let root = egui::ViewportId::ROOT;
    match request {
        WindowRequest::ShowRoot { focus } => {
            engine.set_window_visible(true);
            set_activation_policy(true);
            ctx.send_viewport_cmd_to(root, egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd_to(root, egui::ViewportCommand::Minimized(false));
            if focus {
                ctx.send_viewport_cmd_to(root, egui::ViewportCommand::Focus);
            }
            ctx.request_repaint_of(root);
        }
        WindowRequest::HideRoot { and_settings } => {
            engine.set_window_visible(false);
            if and_settings {
                ctx.send_viewport_cmd_to(
                    settings_window::viewport_id(),
                    egui::ViewportCommand::Visible(false),
                );
            }
            ctx.send_viewport_cmd_to(root, egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd_to(root, egui::ViewportCommand::Visible(false));
            set_activation_policy(false);
        }
        WindowRequest::CloseRoot => ctx.send_viewport_cmd_to(root, egui::ViewportCommand::Close),
        WindowRequest::FocusSettings => {
            ctx.send_viewport_cmd_to(settings_window::viewport_id(), egui::ViewportCommand::Focus)
        }
        WindowRequest::RepaintRoot => ctx.request_repaint_of(root),
        WindowRequest::StopEngine => {
            if let Err(error) = engine.send(Command::Shutdown) {
                log::warn!("Timer command failed: {error}");
            }
        }
    }
}

/// macOS shows the Dock icon and app menu only while the main window is open.
pub(super) fn set_activation_policy(visible: bool) {
    #[cfg(target_os = "macos")]
    if let Some(mtm) = objc2::MainThreadMarker::new() {
        use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
        let app = NSApplication::sharedApplication(mtm);
        let policy = if visible {
            NSApplicationActivationPolicy::Regular
        } else {
            NSApplicationActivationPolicy::Accessory
        };
        if app.activationPolicy() != policy {
            app.setActivationPolicy(policy);
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = visible;
}

#[cfg(test)]
mod tests {
    use super::*;
    use WindowRequest::*;
    use std::time::Duration;

    fn no_shortcut() -> bool {
        false
    }

    #[test]
    fn closing_with_a_tray_hides_the_root_and_settings() {
        let mut windows = WindowLifecycle::new(true);
        windows.open_settings();
        assert_eq!(
            windows.close(true, no_shortcut),
            [HideRoot { and_settings: true }]
        );
        assert!(!windows.root_visible());
    }

    #[test]
    fn a_new_native_close_leaves_the_close_shortcut_unread() {
        let mut windows = WindowLifecycle::new(true);
        let requests = windows.close(true, || panic!("consumed the shortcut as well"));
        assert_eq!(
            requests,
            [HideRoot {
                and_settings: false
            }]
        );
    }

    #[test]
    fn closing_without_a_tray_quits() {
        let mut windows = WindowLifecycle::new(false);
        assert_eq!(windows.close(true, no_shortcut), [StopEngine, CloseRoot]);
    }

    #[test]
    fn closing_while_quitting_finishes_the_quit_instead_of_hiding() {
        let mut windows = WindowLifecycle::new(true);
        assert_eq!(windows.quit(), [StopEngine, CloseRoot]);
        assert_eq!(windows.close(true, no_shortcut), [CloseRoot]);
        assert!(windows.root_visible());
    }

    #[test]
    fn retained_close_event_is_handled_once_and_a_new_close_is_handled() {
        let mut windows = WindowLifecycle::new(true);
        assert!(!windows.close(true, no_shortcut).is_empty());
        windows.show_root(true);
        for _ in 0..100 {
            assert!(windows.close(true, no_shortcut).is_empty());
            assert!(
                windows.root_visible(),
                "a stale close hid the reopened root"
            );
        }
        assert!(windows.close(false, no_shortcut).is_empty());
        assert!(!windows.close(true, no_shortcut).is_empty());
    }

    #[test]
    fn close_shortcut_is_left_unread_while_the_root_is_hidden() {
        let mut windows = WindowLifecycle::new(true);
        assert_eq!(
            windows.close(false, || true),
            [HideRoot {
                and_settings: false
            }]
        );
        let requests = windows.close(false, || panic!("consumed a hidden window's shortcut"));
        assert!(requests.is_empty());
    }

    #[test]
    fn tray_settings_returns_a_hidden_root_without_taking_focus() {
        let mut windows = WindowLifecycle::new(true);
        assert_eq!(windows.open_settings_from_tray(), [RepaintRoot]);
        windows.close(true, no_shortcut);
        assert_eq!(
            windows.open_settings_from_tray(),
            [ShowRoot { focus: false }, RepaintRoot]
        );
        assert!(windows.root_visible());
    }

    #[test]
    fn settings_takes_focus_on_the_second_frame_after_it_has_opened() {
        let mut windows = WindowLifecycle::new(true);
        assert!(windows.settings_frame_shown().is_empty(), "never opened");
        windows.open_settings();
        assert!(windows.settings_frame_shown().is_empty(), "still opening");
        *windows.settings_flags().1 = false;
        assert_eq!(windows.settings_frame_shown(), [RepaintRoot]);
        assert_eq!(windows.settings_frame_shown(), [FocusSettings, RepaintRoot]);
        assert!(windows.settings_frame_shown().is_empty(), "focused once");
    }

    #[test]
    fn settings_focus_waits_for_the_root_to_be_visible() {
        let mut windows = WindowLifecycle::new(true);
        windows.close(true, no_shortcut);
        windows.open_settings();
        *windows.settings_flags().1 = false;
        for _ in 0..3 {
            assert!(windows.settings_frame_shown().is_empty());
        }
        windows.show_root(false);
        assert_eq!(windows.settings_frame_shown(), [RepaintRoot]);
        assert_eq!(windows.settings_frame_shown(), [FocusSettings, RepaintRoot]);
    }

    #[test]
    fn launch_requests_are_refused_while_quitting_shown_otherwise_and_expire() {
        let now = Instant::now();
        let later = now + Duration::from_secs(5);
        let (requests, activations) = mpsc::channel();
        let request = |expires| {
            let (reply, shown) = mpsc::channel();
            requests.send((reply, expires)).unwrap();
            shown
        };
        let answer = |windows: &mut WindowLifecycle| {
            let mut showed = None;
            windows.answer_activations(&activations, now, |requests| showed = Some(requests));
            showed
        };

        let mut windows = WindowLifecycle::new(true);
        let shown = request(later);
        let mut showed = None;
        windows.answer_activations(&activations, now, |requests| {
            // The launch exits on its reply, so the window must be asked to show first.
            assert_eq!(shown.try_recv(), Err(mpsc::TryRecvError::Empty));
            showed = Some(requests);
        });
        assert_eq!(showed, Some(vec![ShowRoot { focus: true }]));
        assert_eq!(shown.try_recv(), Ok(true));
        assert_eq!(answer(&mut windows), None, "nothing pending");
        // A launch that has given up must not open the window afterwards.
        let expired = request(now - Duration::from_millis(1));
        assert_eq!(answer(&mut windows), None);
        assert!(expired.try_recv().is_err());

        // Requested and quit before the same pass: nothing shown, both refused.
        windows.quit();
        let replies = [request(later), request(later)];
        assert_eq!(answer(&mut windows), None);
        assert!(replies.iter().all(|shown| shown.try_recv() == Ok(false)));
    }
}
