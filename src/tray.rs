//! Native tray controls. Construct on the event-loop thread and retain for the app lifetime.
//!
//! This module owns the process-wide muda/tray-icon event handlers. Their weak registrations
//! let the tray (and its engine handle) be dropped without leaving a worker alive.

use std::{
    sync::{Arc, mpsc},
    time::Duration,
};

use crate::engine::{Command, EngineHandle, Snapshot, TimerState};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayAction {
    OpenWindow,
    OpenSettings,
    Quit,
}

pub struct Tray {
    native: native::NativeTray,
    actions: mpsc::Receiver<TrayAction>,
    display: Display,
}

impl Tray {
    /// Must be called on the main thread after the event loop exists.
    /// `wake` can be called from any thread; request a root viewport repaint from it.
    pub fn new(engine: EngineHandle, wake: Arc<dyn Fn() + Send + Sync>) -> anyhow::Result<Self> {
        let display = Display::new(&engine.snapshot(), false);
        let (sender, actions) = mpsc::channel();
        let dispatch = Arc::new(Dispatch {
            engine,
            sender,
            wake,
        });
        let native = native::NativeTray::new(dispatch, &display)?;
        Ok(Self {
            native,
            actions,
            display,
        })
    }

    /// Changes native items only when the state, displayed second, or title preference changes.
    pub fn update(&mut self, snapshot: &Snapshot, show_time_in_menu_bar: bool) {
        let display = Display::new(snapshot, show_time_in_menu_bar);
        if self.display != display {
            self.native.update(&display);
            self.display = display;
        }
    }

    /// Timer commands are dispatched by native callbacks, independently of this drain.
    pub fn poll(&mut self) -> Vec<TrayAction> {
        self.actions.try_iter().collect()
    }
}

const PRESETS: [(u32, &str); 5] = [
    (15, "15 Minutes"),
    (30, "30 Minutes"),
    (45, "45 Minutes"),
    (60, "1 Hour"),
    (90, "90 Minutes"),
];

#[derive(Clone, Copy)]
enum Action {
    Start(u32),
    TogglePause,
    Add,
    Cancel,
    Ui(TrayAction),
}

struct Dispatch {
    engine: EngineHandle,
    sender: mpsc::Sender<TrayAction>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl Dispatch {
    fn run(&self, action: Action) {
        let state = self.engine.snapshot().state;
        let command = match action {
            Action::Ui(action) => {
                if self.sender.send(action).is_ok() {
                    (self.wake)();
                }
                return;
            }
            Action::Start(minutes) if state == TimerState::Idle => Some(Command::Start {
                duration: Duration::from_secs(u64::from(minutes) * 60),
            }),
            Action::TogglePause if state == TimerState::Running => Some(Command::Pause),
            Action::TogglePause if state == TimerState::Paused => Some(Command::Resume),
            Action::Add if matches!(state, TimerState::Running | TimerState::Paused) => {
                Some(Command::AddMinutes(15))
            }
            Action::Cancel if state != TimerState::Idle => Some(Command::Cancel),
            _ => None,
        };
        if let Some(command) = command {
            if let Err(error) = self.engine.send(command) {
                log::warn!("Tray timer command failed: {error}");
            }
            (self.wake)();
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Display {
    state: TimerState,
    time: String,
    show_time: bool,
}

impl Display {
    fn new(snapshot: &Snapshot, show_time: bool) -> Self {
        let seconds = snapshot.menu_seconds();
        Self {
            state: snapshot.state,
            time: if snapshot.state == TimerState::Idle {
                String::new()
            } else {
                format!("{}:{:02}", seconds / 60, seconds % 60)
            },
            show_time,
        }
    }

    fn active(&self) -> bool {
        self.state != TimerState::Idle
    }

    fn controllable(&self) -> bool {
        matches!(self.state, TimerState::Running | TimerState::Paused)
    }

    fn status(&self) -> String {
        match self.state {
            TimerState::Idle => "Nightcap".into(),
            TimerState::Running => format!("Sleeping in {}", self.time),
            TimerState::Paused => format!("Paused, {} left", self.time),
            TimerState::Finishing => "Timer finished".into(),
        }
    }

    fn pause_label(&self) -> &'static str {
        if self.state == TimerState::Paused {
            "Resume"
        } else {
            "Pause"
        }
    }
}

/// Four-by-four supersampling gives a clean crescent at menu-bar sizes.
pub(crate) fn moon_rgba(size: u32) -> Vec<u8> {
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let mut covered = 0_u8;
            for sy in 0..4 {
                for sx in 0..4 {
                    let px = (x as f32 + (sx as f32 + 0.5) / 4.0) / size as f32;
                    let py = (y as f32 + (sy as f32 + 0.5) / 4.0) / size as f32;
                    let outer = (px - 0.5).hypot(py - 0.5) <= 0.41;
                    let cutout = (px - 0.69).hypot(py - 0.33) <= 0.37;
                    covered += u8::from(outer && !cutout);
                }
            }
            // Template images use only alpha on macOS; a light moon suits dark system trays.
            let shade = if cfg!(target_os = "macos") { 0 } else { 230 };
            rgba.extend_from_slice(&[shade, shade, shade, (u16::from(covered) * 255 / 16) as u8]);
        }
    }
    rgba
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
mod native {
    use std::sync::{Mutex, Once, OnceLock, Weak};

    use anyhow::Context as _;
    use tray_icon::{
        Icon, MouseButton, TrayIcon, TrayIconBuilder, TrayIconEvent, TrayIconId,
        menu::{
            IsMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem,
            accelerator::{Accelerator, Code, Modifiers},
        },
    };

    use super::*;

    struct Events {
        dispatch: Arc<Dispatch>,
        tray_id: TrayIconId,
        items: Vec<(MenuId, Action)>,
    }

    fn registrations() -> &'static Mutex<Vec<Weak<Events>>> {
        static REGISTRY: OnceLock<Mutex<Vec<Weak<Events>>>> = OnceLock::new();
        REGISTRY.get_or_init(Mutex::default)
    }

    fn targets() -> Vec<Arc<Events>> {
        let mut registry = registrations().lock().unwrap_or_else(|e| e.into_inner());
        registry.retain(|entry| entry.strong_count() != 0);
        registry.iter().filter_map(Weak::upgrade).collect()
    }

    fn install_handlers() {
        static INSTALL: Once = Once::new();
        INSTALL.call_once(|| {
            MenuEvent::set_event_handler(Some(|event: MenuEvent| {
                for target in targets() {
                    if let Some((_, action)) = target.items.iter().find(|(id, _)| *id == event.id) {
                        target.dispatch.run(*action);
                        break;
                    }
                }
            }));
            TrayIconEvent::set_event_handler(Some(|event: TrayIconEvent| {
                // Menu clicks on macOS/Windows are handled by muda. Windows additionally
                // supports double-clicking the icon to restore the main window.
                if let TrayIconEvent::DoubleClick {
                    id,
                    button: MouseButton::Left,
                    ..
                } = event
                {
                    for target in targets() {
                        if target.tray_id == id {
                            target.dispatch.run(Action::Ui(TrayAction::OpenWindow));
                            break;
                        }
                    }
                }
            }));
        });
    }

    pub(super) struct NativeTray {
        icon: TrayIcon,
        menu: Menu,
        presets: Vec<MenuItem>,
        status: MenuItem,
        pause: MenuItem,
        add: MenuItem,
        cancel: MenuItem,
        active_separator: PredefinedMenuItem,
        active_visible: bool,
        _events: Arc<Events>,
        #[cfg(target_os = "macos")]
        _app_menu: Menu,
    }

    impl NativeTray {
        pub(super) fn new(dispatch: Arc<Dispatch>, display: &Display) -> anyhow::Result<Self> {
            install_handlers();
            let menu = Menu::new();
            menu.append(&MenuItem::new("Start Timer", false, None))?;
            let mut items = Vec::new();
            let mut presets = Vec::new();
            for (minutes, label) in PRESETS {
                let item = MenuItem::new(label, true, None);
                items.push((item.id().clone(), Action::Start(minutes)));
                menu.append(&item)?;
                presets.push(item);
            }
            let status = MenuItem::new(display.status(), false, None);
            let pause = MenuItem::new(display.pause_label(), false, None);
            let add = MenuItem::new("+15 Minutes", false, None);
            let cancel = MenuItem::new("Cancel Timer", false, None);
            items.extend([
                (pause.id().clone(), Action::TogglePause),
                (add.id().clone(), Action::Add),
                (cancel.id().clone(), Action::Cancel),
            ]);
            menu.append(&PredefinedMenuItem::separator())?;
            let modifier = if cfg!(target_os = "macos") {
                Modifiers::META
            } else {
                Modifiers::CONTROL
            };
            for (label, action, accelerator) in [
                ("Open Nightcap", TrayAction::OpenWindow, None),
                (
                    "Settings…",
                    TrayAction::OpenSettings,
                    Some(Accelerator::new(modifier, Code::Comma)),
                ),
                (
                    "Quit Nightcap",
                    TrayAction::Quit,
                    Some(Accelerator::new(modifier, Code::KeyQ)),
                ),
            ] {
                if action == TrayAction::Quit {
                    menu.append(&PredefinedMenuItem::separator())?;
                }
                let item = MenuItem::new(label, true, accelerator);
                items.push((item.id().clone(), Action::Ui(action)));
                menu.append(&item)?;
            }
            // Route native Cmd+Q through the same graceful shutdown as tray Quit.
            // AppKit's default terminate: can otherwise end the process before restore.
            #[cfg(target_os = "macos")]
            let app_menu = {
                use tray_icon::menu::Submenu;
                let app_menu = Menu::new();
                let application = Submenu::new("Nightcap", true);
                let settings = MenuItem::new(
                    "Settings…",
                    true,
                    Some(Accelerator::new(Modifiers::META, Code::Comma)),
                );
                let quit = MenuItem::new(
                    "Quit Nightcap",
                    true,
                    Some(Accelerator::new(Modifiers::META, Code::KeyQ)),
                );
                items.push((settings.id().clone(), Action::Ui(TrayAction::OpenSettings)));
                items.push((quit.id().clone(), Action::Ui(TrayAction::Quit)));
                application.append_items(&[
                    &settings,
                    &PredefinedMenuItem::separator(),
                    &PredefinedMenuItem::hide(Some("Hide Nightcap")),
                    &PredefinedMenuItem::hide_others(None),
                    &PredefinedMenuItem::show_all(None),
                    &PredefinedMenuItem::separator(),
                    &quit,
                ])?;
                let window = Submenu::new("Window", true);
                window.append(&PredefinedMenuItem::close_window(None))?;
                app_menu.append_items(&[&application, &window])?;
                app_menu.init_for_nsapp();
                app_menu
            };
            let icon_data = Icon::from_rgba(moon_rgba(32), 32, 32)?;
            let builder = TrayIconBuilder::new().with_menu(Box::new(menu.clone()));
            #[cfg(target_os = "macos")]
            let builder = builder.with_icon_templated(icon_data);
            #[cfg(target_os = "windows")]
            let builder = builder.with_icon(icon_data);
            let icon = builder
                .with_tooltip("Nightcap")
                .build()
                .context("creating the Nightcap tray icon")?;
            let events = Arc::new(Events {
                dispatch,
                tray_id: icon.id().clone(),
                items,
            });
            registrations()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(Arc::downgrade(&events));
            let mut tray = Self {
                icon,
                menu,
                presets,
                status,
                pause,
                add,
                cancel,
                active_separator: PredefinedMenuItem::separator(),
                active_visible: false,
                _events: events,
                #[cfg(target_os = "macos")]
                _app_menu: app_menu,
            };
            tray.update(display);
            Ok(tray)
        }

        pub(super) fn update(&mut self, display: &Display) {
            if display.active() != self.active_visible {
                let active: [&dyn IsMenuItem; 5] = [
                    &self.active_separator,
                    &self.status,
                    &self.pause,
                    &self.add,
                    &self.cancel,
                ];
                if display.active() {
                    if let Err(error) = self.menu.insert_items(&active, 6) {
                        log::warn!("Updating tray menu failed: {error}");
                        return;
                    }
                } else {
                    for item in active {
                        if let Err(error) = self.menu.remove(item) {
                            log::warn!("Removing tray menu item failed: {error}");
                        }
                    }
                }
                self.active_visible = display.active();
            }
            for preset in &self.presets {
                preset.set_enabled(!display.active());
            }
            self.status.set_text(display.status());
            self.pause.set_text(display.pause_label());
            self.pause.set_enabled(display.controllable());
            self.add.set_enabled(display.controllable());
            self.cancel.set_enabled(display.active());
            if let Err(error) = self.icon.set_tooltip(Some(display.status())) {
                log::warn!("Updating tray tooltip failed: {error}");
            }
            #[cfg(target_os = "macos")]
            self.icon
                .set_title(Some(if display.show_time && display.controllable() {
                    display.time.as_str()
                } else {
                    ""
                }));
        }
    }
}

#[cfg(target_os = "linux")]
mod native {
    use ksni::{
        blocking::{Handle, TrayMethods},
        menu::{MenuItem, StandardItem},
    };

    use super::*;

    struct Service {
        dispatch: Arc<Dispatch>,
        display: Display,
        icon: ksni::Icon,
    }

    impl Service {
        fn item(label: &str, enabled: bool, action: Action) -> MenuItem<Self> {
            StandardItem {
                label: label.into(),
                enabled,
                activate: Box::new(move |service: &mut Self| service.dispatch.run(action)),
                ..Default::default()
            }
            .into()
        }
    }

    impl ksni::Tray for Service {
        fn id(&self) -> String {
            "nightcap".into()
        }
        fn title(&self) -> String {
            "Nightcap".into()
        }
        fn icon_pixmap(&self) -> Vec<ksni::Icon> {
            vec![self.icon.clone()]
        }
        fn tool_tip(&self) -> ksni::ToolTip {
            ksni::ToolTip {
                title: "Nightcap".into(),
                description: self.display.status(),
                ..Default::default()
            }
        }
        fn activate(&mut self, _x: i32, _y: i32) {
            self.dispatch.run(Action::Ui(TrayAction::OpenWindow));
        }
        fn menu_about_to_show(&mut self) {
            self.display = Display::new(&self.dispatch.engine.snapshot(), self.display.show_time);
        }
        fn menu(&self) -> Vec<MenuItem<Self>> {
            let mut menu = vec![Self::item("Start Timer", false, Action::Start(15))];
            menu.extend(PRESETS.map(|(minutes, label)| {
                Self::item(label, !self.display.active(), Action::Start(minutes))
            }));
            if self.display.active() {
                menu.extend([
                    MenuItem::Separator,
                    Self::item(&self.display.status(), false, Action::Cancel),
                    Self::item(
                        self.display.pause_label(),
                        self.display.controllable(),
                        Action::TogglePause,
                    ),
                    Self::item("+15 Minutes", self.display.controllable(), Action::Add),
                    Self::item("Cancel Timer", true, Action::Cancel),
                ]);
            }
            menu.push(MenuItem::Separator);
            menu.push(Self::item(
                "Open Nightcap",
                true,
                Action::Ui(TrayAction::OpenWindow),
            ));
            for (label, action, key) in [
                ("Settings…", TrayAction::OpenSettings, "comma"),
                ("Quit Nightcap", TrayAction::Quit, "Q"),
            ] {
                if action == TrayAction::Quit {
                    menu.push(MenuItem::Separator);
                }
                menu.push(
                    StandardItem {
                        label: label.into(),
                        shortcut: vec![vec!["Control".into(), key.into()]],
                        activate: Box::new(move |service: &mut Self| {
                            service.dispatch.run(Action::Ui(action))
                        }),
                        ..Default::default()
                    }
                    .into(),
                );
            }
            menu
        }
    }

    pub(super) struct NativeTray {
        handle: Handle<Service>,
    }

    impl NativeTray {
        pub(super) fn new(dispatch: Arc<Dispatch>, display: &Display) -> anyhow::Result<Self> {
            let rgba = moon_rgba(32);
            let data = rgba
                .as_chunks::<4>()
                .0
                .iter()
                .flat_map(|p| [p[3], p[0], p[1], p[2]])
                .collect();
            Ok(Self {
                handle: Service {
                    dispatch,
                    display: display.clone(),
                    icon: ksni::Icon {
                        width: 32,
                        height: 32,
                        data,
                    },
                }
                .spawn()?,
            })
        }
        pub(super) fn update(&mut self, display: &Display) {
            self.handle
                .update(|service| service.display = display.clone());
        }
    }

    impl Drop for NativeTray {
        fn drop(&mut self) {
            self.handle.shutdown();
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
mod native {
    use super::*;
    pub(super) struct NativeTray;
    impl NativeTray {
        pub(super) fn new(_dispatch: Arc<Dispatch>, _display: &Display) -> anyhow::Result<Self> {
            anyhow::bail!("Nightcap's tray is unsupported on this platform")
        }
        pub(super) fn update(&mut self, _display: &Display) {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{engine::Engine, platform::fallback, settings::Settings};

    #[test]
    fn display_rounds_up_and_distinguishes_paused_and_finishing() {
        let mut snapshot = Snapshot {
            state: TimerState::Running,
            remaining: Duration::from_millis(754_001),
            ..Default::default()
        };
        assert_eq!(Display::new(&snapshot, true).status(), "Sleeping in 12:35");
        snapshot.state = TimerState::Paused;
        assert_eq!(Display::new(&snapshot, true).pause_label(), "Resume");
        snapshot.state = TimerState::Finishing;
        assert!(!Display::new(&snapshot, true).controllable());
        snapshot.state = TimerState::Idle;
        assert_eq!(Display::new(&snapshot, true).time, "");
    }

    #[test]
    fn icon_has_transparent_opaque_and_antialiased_pixels() {
        let icon = moon_rgba(32);
        assert_eq!(icon.len(), 32 * 32 * 4);
        assert!(icon.as_chunks::<4>().0.iter().any(|pixel| pixel[3] == 0));
        assert!(icon.as_chunks::<4>().0.iter().any(|pixel| pixel[3] == 255));
        assert!(
            icon.as_chunks::<4>()
                .0
                .iter()
                .any(|pixel| pixel[3] > 0 && pixel[3] < 255)
        );
    }

    #[test]
    fn callback_commands_work_without_ui_polling() {
        let (changed_tx, changed_rx) = mpsc::channel();
        let engine = Engine::spawn(
            Settings::default(),
            fallback::backends(),
            Box::new(move || {
                let _ = changed_tx.send(());
            }),
        )
        .unwrap();
        let (sender, receiver) = mpsc::channel();
        let dispatch = Dispatch {
            engine,
            sender,
            wake: Arc::new(|| {}),
        };
        for (action, expected) in [
            (Action::Start(30), TimerState::Running),
            (Action::TogglePause, TimerState::Paused),
            (Action::Add, TimerState::Paused),
            (Action::TogglePause, TimerState::Running),
            (Action::Cancel, TimerState::Idle),
        ] {
            dispatch.run(action);
            changed_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            assert_eq!(dispatch.engine.snapshot().state, expected);
            if matches!(action, Action::Add) {
                assert!(dispatch.engine.snapshot().remaining > Duration::from_secs(44 * 60));
            }
        }
        dispatch.run(Action::Ui(TrayAction::OpenSettings));
        assert_eq!(receiver.try_recv().unwrap(), TrayAction::OpenSettings);
    }
}
