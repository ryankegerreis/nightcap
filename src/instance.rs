//! One Nightcap per desktop session. A later launch asks the running copy to
//! show its window and exits, so two engines never fade the same devices or
//! capture each other's faded levels as the originals to restore.

pub enum Launch {
    /// This process is the session's Nightcap. Keep the guard until exit.
    Primary(Instance),
    /// Another Nightcap is running. `shown` is false when it could not be asked
    /// (other privileges) or did not answer before the timeout.
    Secondary { shown: bool },
}

/// Windows names are per session (`Local\`), matching the tray and windows a user sees.
pub fn acquire() -> Launch {
    imp::acquire("Local\\Nightcap", imp::ACTIVATION_TIMEOUT)
}

pub use imp::Instance;

#[cfg(target_os = "windows")]
mod imp {
    use std::{
        cell::RefCell,
        marker::PhantomData,
        sync::{Once, mpsc},
        thread::{self, JoinHandle},
        time::{Duration, Instant},
    };

    use windows::{
        Win32::{
            Foundation::{
                CloseHandle, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, GetLastError, HANDLE,
                HINSTANCE, HWND, LPARAM, LRESULT, WAIT_ABANDONED, WAIT_OBJECT_0, WPARAM,
            },
            System::{
                LibraryLoader::GetModuleHandleW,
                SystemInformation::GetTickCount64,
                Threading::{
                    CREATE_MUTEX_INITIAL_OWNER, CreateEventW, CreateMutexExW, MUTEX_MODIFY_STATE,
                    ReleaseMutex, SYNCHRONIZATION_SYNCHRONIZE, SetEvent, WaitForSingleObject,
                },
            },
            UI::WindowsAndMessaging::{
                AllowSetForegroundWindow, CreateWindowExW, DefWindowProcW, DestroyWindow,
                DispatchMessageW, FindWindowExW, GetWindowThreadProcessId, HWND_MESSAGE, MSG,
                MsgWaitForMultipleObjects, PM_REMOVE, PeekMessageW, QS_ALLINPUT, RegisterClassExW,
                SMTO_ABORTIFHUNG, SendMessageTimeoutW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP,
                WNDCLASSEXW,
            },
        },
        core::{HSTRING, PCWSTR, w},
    };

    use super::Launch;

    /// How long a later launch waits for the running copy to show itself or exit.
    pub(super) const ACTIVATION_TIMEOUT: Duration = Duration::from_secs(10);

    /// A later launch's request. WPARAM and LPARAM carry the low and high halves
    /// of when it stops waiting, on the system-wide `GetTickCount64` clock, so
    /// time spent queued behind another launch counts (on 32-bit builds too). Its reply (1 shown, 0 refused) answers that call alone, so
    /// no answer can be left over for a different launch.
    const ACTIVATE: u32 = WM_APP + 1;
    const LISTENER_CLASS: PCWSTR = w!("NightcapInstanceListener");
    /// Leaves time for the reply to reach the launch before its own timeout.
    const REPLY_MARGIN: Duration = Duration::from_millis(250);

    /// Owns the session mutex until dropped; keep it until levels are restored.
    /// Mutex ownership belongs to the acquiring thread, so the guard is not `Send`.
    pub struct Instance {
        /// None when the guard is unavailable and the app runs unguarded.
        mutex: Option<isize>,
        /// The listener window's name, which later launches look up.
        name: String,
        /// The listener's stop event and thread.
        listener: Option<(isize, JoinHandle<()>)>,
        _thread_bound: PhantomData<*const ()>,
    }

    fn close(raw: isize) {
        // SAFETY: each raw value is a handle this module opened and closes once.
        let _ = unsafe { CloseHandle(HANDLE(raw as _)) };
    }

    fn primary(mutex: Option<isize>, name: String) -> Launch {
        Launch::Primary(Instance {
            mutex,
            name,
            listener: None,
            _thread_bound: PhantomData,
        })
    }

    pub(super) fn acquire(prefix: &str, timeout: Duration) -> Launch {
        let name = format!("{prefix}.Listener");
        // SAFETY: valid name; default security. Initial ownership applies only
        // when this call creates the mutex.
        let created = unsafe {
            CreateMutexExW(
                None,
                &HSTRING::from(format!("{prefix}.Instance")),
                CREATE_MUTEX_INITIAL_OWNER,
                (SYNCHRONIZATION_SYNCHRONIZE | MUTEX_MODIFY_STATE).0,
            )
        };
        // SAFETY: reads this thread's last-error value, set by CreateMutexExW.
        let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        let mutex = match created {
            Ok(mutex) => mutex.0 as isize,
            Err(error) if error.code() == ERROR_ACCESS_DENIED.to_hresult() => {
                // The running copy has other privileges, e.g. it was started elevated.
                log::warn!("Nightcap is already running with different privileges");
                return Launch::Secondary { shown: false };
            }
            Err(error) => {
                // Never refuse to start because the guard itself is unavailable.
                log::warn!("Single-instance check unavailable: {error}");
                return primary(None, name);
            }
        };
        if existed {
            match take_over(mutex, &name, timeout) {
                Handoff::Owner => log::info!("The running Nightcap exited; starting instead"),
                Handoff::Shown => {
                    close(mutex);
                    return Launch::Secondary { shown: true };
                }
                Handoff::Unanswered => {
                    log::warn!("The running Nightcap did not respond");
                    close(mutex);
                    return Launch::Secondary { shown: false };
                }
            }
        }
        primary(Some(mutex), name)
    }

    enum Handoff {
        /// The running copy showed its window.
        Shown,
        /// It exited or crashed first; this launch now owns the mutex.
        Owner,
        Unanswered,
    }

    /// Asks each listener to show its window. One that refuses is quitting, so
    /// keep waiting: either its replacement (another launch that took over)
    /// listens and accepts, or this launch gets the mutex and starts itself.
    fn take_over(mutex: isize, name: &str, timeout: Duration) -> Handoff {
        let deadline = Instant::now() + timeout;
        let name = HSTRING::from(name);
        // Listeners that refused, by window and process: a replacement in
        // another process is asked even if Windows reuses the window handle.
        let mut refused = Vec::new();
        loop {
            let mut after = None;
            // SAFETY: message-only window lookups.
            while let Ok(listener) =
                unsafe { FindWindowExW(Some(HWND_MESSAGE), after, LISTENER_CLASS, &name) }
            {
                after = Some(listener);
                let mut process = 0;
                // SAFETY: reads the owner of a window found above.
                unsafe { GetWindowThreadProcessId(listener, Some(&mut process)) };
                if refused.contains(&(listener.0, process)) {
                    continue;
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let milliseconds = remaining.as_millis().min(u128::from(u32::MAX - 1)) as u32;
                let mut shown = 0;
                // SAFETY: lets only the listener's process take the foreground,
                // then makes a bounded synchronous call.
                let delivered = unsafe {
                    let _ = AllowSetForegroundWindow(process);
                    let stops = GetTickCount64() + u64::from(milliseconds);
                    SendMessageTimeoutW(
                        listener,
                        ACTIVATE,
                        WPARAM(stops as u32 as usize),
                        LPARAM((stops >> 32) as isize),
                        SMTO_ABORTIFHUNG,
                        milliseconds,
                        Some(&mut shown),
                    )
                };
                if delivered.0 != 0 {
                    if shown == 1 {
                        return Handoff::Shown;
                    }
                    refused.push((listener.0, process));
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            let poll = remaining.min(Duration::from_millis(100)).as_millis() as u32;
            // SAFETY: waits on this launch's own mutex handle.
            let signalled = unsafe { WaitForSingleObject(HANDLE(mutex as _), poll) };
            if signalled == WAIT_OBJECT_0 || signalled == WAIT_ABANDONED {
                return Handoff::Owner;
            }
            if remaining.is_zero() {
                return Handoff::Unanswered;
            }
        }
    }

    type Callback = Box<dyn Fn(Duration) -> bool>;

    thread_local! {
        static ACTIVATE_CALLBACK: RefCell<Option<Callback>> = RefCell::new(None);
    }

    unsafe extern "system" fn listener_procedure(
        window: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if message == ACTIVATE {
            // SAFETY: reads the system tick count, the clock the launch used.
            let stops = (lparam.0 as u64) << 32 | u64::from(wparam.0 as u32);
            let left = stops.saturating_sub(unsafe { GetTickCount64() });
            let allowed = Duration::from_millis(left).saturating_sub(REPLY_MARGIN);
            // A launch that has (nearly) given up is refused rather than shown late.
            let shown = !allowed.is_zero()
                && ACTIVATE_CALLBACK
                    .with_borrow(|activate| activate.as_ref().is_some_and(|f| f(allowed)));
            return LRESULT(isize::from(shown));
        }
        // SAFETY: forwards this window's other messages unchanged.
        unsafe { DefWindowProcW(window, message, wparam, lparam) }
    }

    /// A message-only window on the calling thread, which must pump its messages.
    fn listener_window(name: &str) -> windows::core::Result<HWND> {
        // SAFETY: plain Win32 calls; the window is destroyed by its own thread.
        unsafe {
            let instance = HINSTANCE(GetModuleHandleW(None)?.0);
            static REGISTER: Once = Once::new();
            REGISTER.call_once(|| {
                let class = WNDCLASSEXW {
                    cbSize: size_of::<WNDCLASSEXW>() as u32,
                    lpfnWndProc: Some(listener_procedure),
                    hInstance: instance,
                    lpszClassName: LISTENER_CLASS,
                    ..Default::default()
                };
                if RegisterClassExW(&class) == 0 {
                    log::warn!(
                        "Registering the launch listener failed: {}",
                        windows::core::Error::from_thread()
                    );
                }
            });
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                LISTENER_CLASS,
                &HSTRING::from(name),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                Some(instance),
                None,
            )
        }
    }

    impl Instance {
        /// Answers each later launch on a background thread. `activate` gets the
        /// time that launch will wait; it should return true once the window has
        /// been asked to show, and false when it cannot be (the app is quitting)
        /// or no answer came in time. A refused launch starts its own copy once
        /// this one exits. Focus remains subject to Windows' foreground rules.
        pub fn on_activate(&mut self, activate: impl Fn(Duration) -> bool + Send + 'static) {
            if self.mutex.is_none() || self.listener.is_some() {
                return;
            }
            // SAFETY: unnamed manual-reset event; independent of message-queue capacity.
            let stop = match unsafe { CreateEventW(None, true, false, None) } {
                Ok(stop) => stop.0 as isize,
                Err(error) => {
                    log::warn!("Could not listen for later launches: {error}");
                    return;
                }
            };
            let name = self.name.clone();
            let (ready, started) = mpsc::channel();
            let listener = thread::Builder::new()
                .name("nightcap-instance".into())
                .spawn(move || {
                    ACTIVATE_CALLBACK.set(Some(Box::new(activate)));
                    let window = match listener_window(&name) {
                        Ok(window) => window,
                        Err(error) => {
                            log::warn!("Could not listen for later launches: {error}");
                            let _ = ready.send(false);
                            return;
                        }
                    };
                    let _ = ready.send(true);
                    let mut message = MSG::default();
                    // SAFETY: pumps this thread's queue, including sent messages,
                    // until the stop event is set.
                    unsafe {
                        loop {
                            let woke = MsgWaitForMultipleObjects(
                                Some(&[HANDLE(stop as _)]),
                                false,
                                u32::MAX,
                                QS_ALLINPUT,
                            );
                            // Stopped, or the wait failed and would otherwise spin.
                            if woke.0 != WAIT_OBJECT_0.0 + 1 {
                                break;
                            }
                            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                                DispatchMessageW(&message);
                            }
                        }
                        let _ = DestroyWindow(window);
                    }
                });
            match listener {
                Ok(listener) => {
                    if started.recv() == Ok(true) {
                        self.listener = Some((stop, listener));
                    } else {
                        let _ = listener.join();
                        close(stop);
                    }
                }
                Err(error) => {
                    log::warn!("Could not listen for later launches: {error}");
                    close(stop);
                }
            }
        }
    }

    impl Drop for Instance {
        fn drop(&mut self) {
            if let Some((stop, listener)) = self.listener.take() {
                // SAFETY: the stop event is open until after the join.
                let _ = unsafe { SetEvent(HANDLE(stop as _)) };
                let _ = listener.join();
                close(stop);
            }
            if let Some(mutex) = self.mutex.take() {
                // SAFETY: the guard is not Send, so this is the owning thread.
                let _ = unsafe { ReleaseMutex(HANDLE(mutex as _)) };
                close(mutex);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use std::sync::mpsc;

        use windows::Win32::Security::{
            ACL, ACL_REVISION, InitializeAcl, InitializeSecurityDescriptor, PSECURITY_DESCRIPTOR,
            SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR, SetSecurityDescriptorDacl,
        };

        use super::*;

        const TIMEOUT: Duration = Duration::from_secs(5);

        fn prefix(test: &str) -> String {
            format!("Local\\Nightcap.Test.{}.{test}", std::process::id())
        }

        /// A launch on its own thread, as a separate process would be (Win32
        /// mutexes are recursive per thread). It must become the primary, may
        /// listen, then holds the mutex until `exit` is sent or dropped.
        fn run_primary(
            prefix: &str,
            listen: Option<bool>,
            crash: bool,
        ) -> (mpsc::Sender<()>, JoinHandle<()>) {
            let prefix = prefix.to_owned();
            let (ready_tx, ready) = mpsc::channel();
            let (exit, exit_rx) = mpsc::channel::<()>();
            let thread = std::thread::spawn(move || {
                let Launch::Primary(mut instance) = acquire(&prefix, TIMEOUT) else {
                    panic!("{prefix} must start as the primary");
                };
                if let Some(accept) = listen {
                    instance.on_activate(move |_| accept);
                }
                ready_tx.send(()).unwrap();
                let _ = exit_rx.recv();
                if crash {
                    // The thread ends while owning the mutex, abandoning it.
                    std::mem::forget(instance);
                }
            });
            ready.recv_timeout(TIMEOUT).unwrap();
            (exit, thread)
        }

        /// A later launch: "primary" (it listens and accepts, then exits after
        /// `hold`), "shown" or "failed".
        fn later_launch(prefix: &str, hold: Duration) -> JoinHandle<(&'static str, Duration)> {
            let prefix = prefix.to_owned();
            std::thread::spawn(move || {
                let started = Instant::now();
                let outcome = match acquire(&prefix, TIMEOUT) {
                    Launch::Primary(mut instance) => {
                        let took = started.elapsed();
                        instance.on_activate(|_| true);
                        std::thread::sleep(hold);
                        return ("primary", took);
                    }
                    Launch::Secondary { shown: true } => "shown",
                    Launch::Secondary { shown: false } => "failed",
                };
                (outcome, started.elapsed())
            })
        }

        #[test]
        fn later_launches_activate_the_first_and_the_guard_is_released_on_exit() {
            let prefix = prefix("activate");
            let (exit, first) = run_primary(&prefix, Some(true), false);
            for launch in [
                later_launch(&prefix, Duration::ZERO),
                later_launch(&prefix, Duration::ZERO),
            ] {
                let (outcome, took) = launch.join().unwrap();
                assert_eq!(outcome, "shown");
                assert!(took < TIMEOUT);
            }
            drop(exit);
            first.join().unwrap();
            let (exit, first) = run_primary(&prefix, None, false);
            drop(exit);
            first.join().unwrap();
        }

        #[test]
        fn concurrent_launches_before_listening_are_each_answered() {
            let prefix = prefix("early");
            let (ready_tx, ready) = mpsc::channel();
            let (exit, exit_rx) = mpsc::channel::<()>();
            let first = {
                let prefix = prefix.clone();
                std::thread::spawn(move || {
                    let Launch::Primary(mut instance) = acquire(&prefix, TIMEOUT) else {
                        panic!("the first launch must own the instance");
                    };
                    ready_tx.send(()).unwrap();
                    // Later launches arrive before this copy starts listening.
                    std::thread::sleep(Duration::from_millis(200));
                    let (shown, activations) = mpsc::channel();
                    instance.on_activate(move |_| shown.send(()).is_ok());
                    let _ = exit_rx.recv();
                    activations.try_iter().count()
                })
            };
            ready.recv_timeout(TIMEOUT).unwrap();
            for launch in [
                later_launch(&prefix, Duration::ZERO),
                later_launch(&prefix, Duration::ZERO),
            ] {
                let (outcome, took) = launch.join().unwrap();
                assert_eq!(outcome, "shown");
                assert!(took < TIMEOUT);
            }
            drop(exit);
            assert_eq!(first.join().unwrap(), 2);
        }

        #[test]
        fn an_expired_request_is_refused_without_reaching_the_app() {
            let prefix = prefix("expired");
            let (calls_tx, calls) = mpsc::channel();
            let (ready_tx, ready) = mpsc::channel();
            let (exit, exit_rx) = mpsc::channel::<()>();
            let first = {
                let prefix = prefix.clone();
                std::thread::spawn(move || {
                    let Launch::Primary(mut instance) = acquire(&prefix, TIMEOUT) else {
                        panic!("the first launch must own the instance");
                    };
                    instance.on_activate(move |_| calls_tx.send(()).is_ok());
                    ready_tx.send(()).unwrap();
                    let _ = exit_rx.recv();
                })
            };
            ready.recv_timeout(TIMEOUT).unwrap();
            let name = HSTRING::from(format!("{prefix}.Listener"));
            // SAFETY: looks up the test's listener and sends it a bounded request.
            let reply = |stops: u64| unsafe {
                let listener =
                    FindWindowExW(Some(HWND_MESSAGE), None, LISTENER_CLASS, &name).unwrap();
                let mut shown = 9;
                let delivered = SendMessageTimeoutW(
                    listener,
                    ACTIVATE,
                    WPARAM(stops as u32 as usize),
                    LPARAM((stops >> 32) as isize),
                    SMTO_ABORTIFHUNG,
                    5000,
                    Some(&mut shown),
                );
                assert_ne!(delivered.0, 0, "the listener received the request");
                shown
            };
            // SAFETY: reads the system tick count.
            let now = unsafe { GetTickCount64() };
            assert_eq!(reply(now.saturating_sub(1)), 0);
            assert_eq!(reply(now + 100), 0, "less time left than the reply margin");
            assert_eq!(calls.try_iter().count(), 0);
            // A live deadline, split across both parameters, reaches the app.
            assert_eq!(reply(now + 5000), 1);
            assert_eq!(calls.try_iter().count(), 1);
            drop(exit);
            first.join().unwrap();
        }

        #[test]
        fn a_request_dispatched_after_its_launch_gave_up_is_not_shown() {
            let prefix = prefix("late");
            let (calls_tx, calls) = mpsc::channel();
            let (ready_tx, ready) = mpsc::channel();
            let (exit, exit_rx) = mpsc::channel::<()>();
            let first = {
                let prefix = prefix.clone();
                std::thread::spawn(move || {
                    let Launch::Primary(mut instance) = acquire(&prefix, TIMEOUT) else {
                        panic!("the first launch must own the instance");
                    };
                    // A busy UI: each request takes longer than the next launch waits.
                    instance.on_activate(move |_| {
                        calls_tx.send(()).unwrap();
                        std::thread::sleep(Duration::from_millis(700));
                        true
                    });
                    ready_tx.send(()).unwrap();
                    let _ = exit_rx.recv();
                })
            };
            ready.recv_timeout(TIMEOUT).unwrap();
            let patient = later_launch(&prefix, Duration::ZERO);
            std::thread::sleep(Duration::from_millis(100));
            let impatient = {
                let prefix = prefix.clone();
                std::thread::spawn(move || {
                    matches!(
                        acquire(&prefix, Duration::from_millis(400)),
                        Launch::Secondary { shown: false }
                    )
                })
            };
            assert!(
                impatient.join().unwrap(),
                "the impatient launch reports failure"
            );
            assert_eq!(patient.join().unwrap().0, "shown");
            // Give the queued, expired request time to reach the listener.
            std::thread::sleep(Duration::from_millis(800));
            drop(exit);
            first.join().unwrap();
            assert_eq!(
                calls.try_iter().count(),
                1,
                "the expired request never reached the UI"
            );
        }

        #[test]
        fn a_launch_during_quit_or_after_a_crash_starts_once_the_first_is_gone() {
            for crash in [false, true] {
                let prefix = prefix(if crash { "crash" } else { "quit" });
                // A quitting app refuses to show its window.
                let (exit, first) = run_primary(&prefix, Some(false), crash);
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(300));
                    drop(exit);
                });
                // run_primary asserts that the later launch becomes the primary.
                let (taken, second) = run_primary(&prefix, None, false);
                drop(taken);
                second.join().unwrap();
                first.join().unwrap();
            }
        }

        #[test]
        fn launches_refused_during_quit_start_one_copy_that_shows_for_the_other() {
            let prefix = prefix("refused");
            let (exit, first) = run_primary(&prefix, Some(false), false);
            let launches = [
                later_launch(&prefix, Duration::from_millis(500)),
                later_launch(&prefix, Duration::from_millis(500)),
            ];
            std::thread::sleep(Duration::from_millis(300));
            drop(exit);
            first.join().unwrap();
            let mut outcomes: Vec<_> = launches
                .into_iter()
                .map(|launch| {
                    let (outcome, took) = launch.join().unwrap();
                    assert!(took < TIMEOUT);
                    outcome
                })
                .collect();
            outcomes.sort();
            assert_eq!(outcomes, ["primary", "shown"]);
        }

        #[test]
        fn a_launch_that_took_over_leaves_no_answer_for_the_next_launch() {
            // Regression: a shared reply counter once let a later launch consume
            // an earlier launch's leftover answer and exit while the app quit.
            let prefix = prefix("takeover");
            let (exit, first) = run_primary(&prefix, None, false);
            let (handover, waiting) = mpsc::channel();
            let second = {
                let prefix = prefix.clone();
                std::thread::spawn(move || {
                    let Launch::Primary(mut instance) = acquire(&prefix, TIMEOUT) else {
                        panic!("the second launch must take over");
                    };
                    // Now quitting: refuse every later launch.
                    instance.on_activate(|_| false);
                    handover.send(()).unwrap();
                    std::thread::sleep(Duration::from_millis(300));
                })
            };
            std::thread::sleep(Duration::from_millis(100));
            drop(exit);
            first.join().unwrap();
            waiting.recv_timeout(TIMEOUT).unwrap();
            let started = Instant::now();
            let Launch::Primary(third) = acquire(&prefix, TIMEOUT) else {
                panic!("the third launch must start once the quitting copy exits");
            };
            assert!(started.elapsed() < TIMEOUT);
            assert!(third.mutex.is_some());
            second.join().unwrap();
        }

        #[test]
        fn an_instance_this_user_cannot_open_is_still_treated_as_running() {
            let prefix = prefix("denied");
            // SAFETY: an empty DACL denies every access but the creator's own handle.
            let mutex = unsafe {
                let mut acl = ACL::default();
                InitializeAcl(&mut acl, size_of::<ACL>() as u32, ACL_REVISION).unwrap();
                let mut descriptor = SECURITY_DESCRIPTOR::default();
                let pointer = PSECURITY_DESCRIPTOR(&mut descriptor as *mut _ as _);
                InitializeSecurityDescriptor(pointer, 1).unwrap();
                SetSecurityDescriptorDacl(pointer, true, Some(&acl), false).unwrap();
                let attributes = SECURITY_ATTRIBUTES {
                    nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                    lpSecurityDescriptor: pointer.0,
                    bInheritHandle: false.into(),
                };
                CreateMutexExW(
                    Some(&attributes),
                    &HSTRING::from(format!("{prefix}.Instance")),
                    0,
                    SYNCHRONIZATION_SYNCHRONIZE.0,
                )
                .unwrap()
            };
            let started = Instant::now();
            assert!(matches!(
                acquire(&prefix, TIMEOUT),
                Launch::Secondary { shown: false }
            ));
            assert!(started.elapsed() < TIMEOUT);
            close(mutex.0 as isize);
        }
    }
}

#[cfg(not(target_os = "windows"))]
mod imp {
    use std::time::Duration;

    use super::Launch;

    /// macOS app bundles are single-instance through LaunchServices; Linux has no guard yet.
    pub struct Instance;

    pub(super) const ACTIVATION_TIMEOUT: Duration = Duration::ZERO;

    pub(super) fn acquire(_prefix: &str, _timeout: Duration) -> Launch {
        Launch::Primary(Instance)
    }

    impl Instance {
        pub fn on_activate(&mut self, _activate: impl Fn(Duration) -> bool + Send + 'static) {}
    }
}
