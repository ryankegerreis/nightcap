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
/// Linux uses the user's runtime directory, so it is per user: a second graphical
/// session of the same user shows the first session's window instead of starting.
pub fn acquire() -> Launch {
    #[cfg(target_os = "windows")]
    {
        imp::acquire("Local\\Nightcap", imp::ACTIVATION_TIMEOUT)
    }
    #[cfg(target_os = "linux")]
    {
        match imp::runtime_base(std::env::var_os("XDG_RUNTIME_DIR")) {
            Ok(base) => imp::acquire(&base, imp::ACTIVATION_TIMEOUT),
            Err(reason) => {
                // Never refuse to start because the guard itself is unavailable.
                log::warn!("Single-instance check unavailable: {reason}");
                Launch::Primary(imp::Instance::unguarded())
            }
        }
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        imp::acquire("Nightcap", imp::ACTIVATION_TIMEOUT)
    }
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

/// An advisory lock on `<base>.lock` marks the running copy. It listens on the Unix
/// socket `<base>.sock`, where a later launch sends when it stops waiting and reads
/// one byte back: 1 shown, 0 refused. The kernel drops the lock when a copy exits or
/// crashes, so a stale socket never outlives its owner's claim.
#[cfg(target_os = "linux")]
mod imp {
    use std::{
        ffi::OsString,
        fs::{self, File, OpenOptions, Permissions, TryLockError},
        io::{self, Read, Write},
        os::unix::{
            fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
        path::{Path, PathBuf},
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, JoinHandle},
        time::{Duration, Instant},
    };

    use super::Launch;

    /// How long a later launch waits for the running copy to show itself or exit.
    pub(super) const ACTIVATION_TIMEOUT: Duration = Duration::from_secs(10);
    /// Leaves time for the reply to reach the launch before its own timeout.
    const REPLY_MARGIN: Duration = Duration::from_millis(250);
    /// How often a waiting launch asks again, or checks whether the copy exited.
    const POLL: Duration = Duration::from_millis(100);
    /// A client that connects but sends nothing cannot hold up the launches behind it.
    const REQUEST_TIMEOUT: Duration = Duration::from_secs(1);
    /// Owner read and write. The sticky bit keeps the runtime directory's periodic
    /// clean-up from removing the files while Nightcap runs (XDG Base Directory
    /// specification): a removed lock path would let a second copy lock a new file.
    const MODE: u32 = 0o1600;

    /// Where the guard's files go: the runtime directory, if it is what the XDG
    /// specification promises (absolute, owned by this user, private to them).
    pub(super) fn runtime_base(dir: Option<OsString>) -> Result<PathBuf, String> {
        let dir = PathBuf::from(
            dir.filter(|dir| !dir.is_empty())
                .ok_or("XDG_RUNTIME_DIR is not set")?,
        );
        if !dir.is_absolute() {
            return Err(format!("XDG_RUNTIME_DIR {} is not absolute", dir.display()));
        }
        let metadata = fs::metadata(&dir)
            .map_err(|error| format!("XDG_RUNTIME_DIR {}: {error}", dir.display()))?;
        let private = metadata.is_dir()
            && metadata.uid() == rustix::process::getuid().as_raw()
            && metadata.mode() & 0o077 == 0;
        if !private {
            return Err(format!(
                "XDG_RUNTIME_DIR {} is not a directory private to this user",
                dir.display()
            ));
        }
        Ok(dir.join("nightcap"))
    }

    /// The system-wide monotonic clock, which every process reads alike. It stands
    /// still while the machine sleeps and is not moved by wall-clock changes.
    fn monotonic() -> Duration {
        let now = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
        Duration::new(
            u64::try_from(now.tv_sec).unwrap_or(0),
            u32::try_from(now.tv_nsec).unwrap_or(0),
        )
    }

    /// Connects without waiting: a listener whose queue is full is busy right now.
    fn connect(socket: &Path) -> io::Result<UnixStream> {
        use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType};
        let fd = rustix::net::socket_with(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::NONBLOCK | SocketFlags::CLOEXEC,
            None,
        )?;
        rustix::net::connect(&fd, &SocketAddrUnix::new(socket)?)?;
        let stream = UnixStream::from(fd);
        stream.set_nonblocking(false)?;
        Ok(stream)
    }

    /// Holds the lock until dropped; keep it until levels are restored.
    pub struct Instance {
        /// None when the guard is unavailable and the app runs unguarded.
        lock: Option<File>,
        socket: PathBuf,
        /// The listener's stop flag and thread.
        listener: Option<(Arc<AtomicBool>, JoinHandle<()>)>,
    }

    impl Instance {
        pub(super) fn unguarded() -> Self {
            Self {
                lock: None,
                socket: PathBuf::new(),
                listener: None,
            }
        }
    }

    fn primary(lock: File, socket: PathBuf) -> Launch {
        // The file may predate the sticky bit, or have been made by another tool.
        if let Err(error) = lock.set_permissions(Permissions::from_mode(MODE)) {
            log::warn!("Could not protect the instance lock from clean-up: {error}");
        }
        Launch::Primary(Instance {
            lock: Some(lock),
            socket,
            listener: None,
        })
    }

    /// Ok(false) while another copy holds the lock.
    fn try_lock(file: &File) -> io::Result<bool> {
        match file.try_lock() {
            Ok(()) => Ok(true),
            Err(TryLockError::WouldBlock) => Ok(false),
            Err(TryLockError::Error(error)) => Err(error),
        }
    }

    pub(super) fn acquire(base: &Path, timeout: Duration) -> Launch {
        let socket = base.with_extension("sock");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(MODE)
            .open(base.with_extension("lock"));
        let lock = match lock {
            Ok(lock) => lock,
            Err(error) => {
                // Never refuse to start because the guard itself is unavailable.
                log::warn!("Single-instance check unavailable: {error}");
                return Launch::Primary(Instance::unguarded());
            }
        };
        let deadline = Instant::now() + timeout;
        loop {
            match try_lock(&lock) {
                Ok(true) => return primary(lock, socket),
                Ok(false) => {}
                Err(error) => {
                    log::warn!("Single-instance check unavailable: {error}");
                    return Launch::Primary(Instance::unguarded());
                }
            }
            // A copy that refuses is quitting, and one not listening yet is starting:
            // either way, ask again until it shows itself or exits.
            if ask(&socket, deadline) == Some(true) {
                return Launch::Secondary { shown: true };
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                log::warn!("The running Nightcap did not respond");
                return Launch::Secondary { shown: false };
            }
            thread::sleep(remaining.min(POLL));
        }
    }

    /// Whether the running copy showed its window; None when it could not be asked.
    fn ask(socket: &Path, deadline: Instant) -> Option<bool> {
        let mut stream = connect(socket).ok()?;
        // When this launch stops waiting, on the clock both processes share, so time
        // spent queued behind another launch counts.
        let stops = monotonic().checked_add(time_left(deadline).ok()?)?;
        let stops = u64::try_from(stops.as_nanos()).ok()?;
        write_by(&mut stream, &stops.to_le_bytes(), deadline).ok()?;
        let mut reply = [0];
        read_by(&mut stream, &mut reply, deadline).ok()?;
        Some(reply[0] == 1)
    }

    /// Answers one launch. One that has (nearly) given up is refused rather than shown late.
    fn answer(mut stream: UnixStream, activate: &dyn Fn(Duration) -> bool) {
        let mut request = [0; 8];
        if read_by(&mut stream, &mut request, Instant::now() + REQUEST_TIMEOUT).is_err() {
            return;
        }
        let left = Duration::from_nanos(u64::from_le_bytes(request))
            .saturating_sub(monotonic())
            .min(ACTIVATION_TIMEOUT);
        let allowed = left.saturating_sub(REPLY_MARGIN);
        let shown = !allowed.is_zero() && activate(allowed);
        let _ = write_by(
            &mut stream,
            &[u8::from(shown)],
            Instant::now() + REQUEST_TIMEOUT,
        );
    }

    /// The time until `deadline`; an error once it has passed, never "no timeout".
    fn time_left(deadline: Instant) -> io::Result<Duration> {
        Some(deadline.saturating_duration_since(Instant::now()))
            .filter(|left| !left.is_zero())
            .ok_or_else(|| io::ErrorKind::TimedOut.into())
    }

    /// Reads all of `bytes` by `deadline`, however slowly they arrive.
    fn read_by(stream: &mut UnixStream, mut bytes: &mut [u8], deadline: Instant) -> io::Result<()> {
        while !bytes.is_empty() {
            stream.set_read_timeout(Some(time_left(deadline)?))?;
            match stream.read(bytes) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(read) => bytes = &mut bytes[read..],
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// Writes all of `bytes` by `deadline`.
    fn write_by(stream: &mut UnixStream, mut bytes: &[u8], deadline: Instant) -> io::Result<()> {
        while !bytes.is_empty() {
            stream.set_write_timeout(Some(time_left(deadline)?))?;
            match stream.write(bytes) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(written) => bytes = &bytes[written..],
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    impl Instance {
        /// Answers each later launch on a background thread. `activate` gets the
        /// time that launch will wait; it should return true once the window has
        /// been asked to show, and false when it cannot be (the app is quitting)
        /// or no answer came in time. A refused launch starts its own copy once
        /// this one exits. Focus remains subject to the window manager.
        pub fn on_activate(&mut self, activate: impl Fn(Duration) -> bool + Send + 'static) {
            if self.lock.is_none() || self.listener.is_some() {
                return;
            }
            // A copy that crashed leaves its socket behind. Holding the lock makes
            // the path this copy's to replace.
            let _ = fs::remove_file(&self.socket);
            let listener = match UnixListener::bind(&self.socket) {
                Ok(listener) => listener,
                Err(error) => {
                    log::warn!("Could not listen for later launches: {error}");
                    return;
                }
            };
            if let Err(error) = fs::set_permissions(&self.socket, Permissions::from_mode(MODE)) {
                log::warn!("Could not protect the launch socket from clean-up: {error}");
            }
            let stop = Arc::new(AtomicBool::new(false));
            let stopping = Arc::clone(&stop);
            let thread = thread::Builder::new()
                .name("nightcap-instance".into())
                .spawn(move || {
                    for stream in listener.incoming() {
                        if stopping.load(Ordering::Acquire) {
                            break;
                        }
                        if let Ok(stream) = stream {
                            answer(stream, &activate);
                        }
                    }
                });
            match thread {
                Ok(thread) => self.listener = Some((stop, thread)),
                Err(error) => {
                    log::warn!("Could not listen for later launches: {error}");
                    let _ = fs::remove_file(&self.socket);
                }
            }
        }
    }

    impl Drop for Instance {
        fn drop(&mut self) {
            if let Some((stop, listener)) = self.listener.take() {
                stop.store(true, Ordering::Release);
                // Wake the blocked accept. If that cannot connect at once (the socket
                // was removed, or its queue is full), the thread ends with the process.
                if connect(&self.socket).is_ok() {
                    let _ = listener.join();
                }
                // Before the lock is released, so a successor's socket is never removed.
                let _ = fs::remove_file(&self.socket);
            }
            // Dropping the file afterwards releases the lock.
        }
    }

    #[cfg(test)]
    mod tests {
        use std::sync::mpsc;

        use super::*;

        const TIMEOUT: Duration = Duration::from_secs(5);

        fn base(dir: &tempfile::TempDir) -> PathBuf {
            dir.path().join("nightcap")
        }

        /// A launch that must become the primary, may listen (accepting or
        /// refusing), then holds the lock until `exit` is sent or dropped.
        fn run_primary(base: &Path, listen: Option<bool>) -> (mpsc::Sender<()>, JoinHandle<()>) {
            let base = base.to_owned();
            let (ready_tx, ready) = mpsc::channel();
            let (exit, exit_rx) = mpsc::channel::<()>();
            let thread = thread::spawn(move || {
                let Launch::Primary(mut instance) = acquire(&base, TIMEOUT) else {
                    panic!("{} must start as the primary", base.display());
                };
                if let Some(accept) = listen {
                    instance.on_activate(move |_| accept);
                }
                ready_tx.send(()).unwrap();
                let _ = exit_rx.recv();
            });
            ready.recv_timeout(TIMEOUT).unwrap();
            (exit, thread)
        }

        /// A later launch: "primary" (it listens and accepts, then exits after
        /// `hold`), "shown" or "failed".
        fn later_launch(base: &Path, hold: Duration) -> JoinHandle<&'static str> {
            let base = base.to_owned();
            thread::spawn(move || match acquire(&base, TIMEOUT) {
                Launch::Primary(mut instance) => {
                    instance.on_activate(|_| true);
                    thread::sleep(hold);
                    "primary"
                }
                Launch::Secondary { shown: true } => "shown",
                Launch::Secondary { shown: false } => "failed",
            })
        }

        fn outcome(launch: Launch) -> &'static str {
            match launch {
                Launch::Primary(_) => "primary",
                Launch::Secondary { shown: true } => "shown",
                Launch::Secondary { shown: false } => "failed",
            }
        }

        fn request(base: &Path, stops: u64) -> u8 {
            let mut stream = UnixStream::connect(base.with_extension("sock")).unwrap();
            stream.set_read_timeout(Some(TIMEOUT)).unwrap();
            stream.write_all(&stops.to_le_bytes()).unwrap();
            let mut reply = [9];
            stream.read_exact(&mut reply).unwrap();
            reply[0]
        }

        fn in_ms(milliseconds: u64) -> u64 {
            (monotonic() + Duration::from_millis(milliseconds)).as_nanos() as u64
        }

        #[test]
        fn later_launches_activate_the_first_and_the_guard_is_released_on_exit() {
            let dir = tempfile::tempdir().unwrap();
            let base = base(&dir);
            let (exit, first) = run_primary(&base, Some(true));
            for _ in 0..2 {
                let started = Instant::now();
                assert_eq!(outcome(acquire(&base, TIMEOUT)), "shown");
                assert!(started.elapsed() < TIMEOUT);
            }
            drop(exit);
            first.join().unwrap();
            assert!(!base.with_extension("sock").exists(), "socket left behind");
            let (exit, first) = run_primary(&base, None);
            drop(exit);
            first.join().unwrap();
        }

        #[test]
        fn the_lock_and_socket_are_protected_from_runtime_clean_up() {
            let dir = tempfile::tempdir().unwrap();
            let base = base(&dir);
            fs::write(base.with_extension("lock"), b"").unwrap();
            let (exit, first) = run_primary(&base, Some(true));
            for path in [base.with_extension("lock"), base.with_extension("sock")] {
                let mode = fs::metadata(&path).unwrap().mode() & 0o7777;
                assert_eq!(mode, MODE, "{}", path.display());
            }
            drop(exit);
            first.join().unwrap();
        }

        #[test]
        fn only_a_private_absolute_runtime_directory_is_used() {
            let dir = tempfile::tempdir().unwrap();
            fs::set_permissions(dir.path(), Permissions::from_mode(0o700)).unwrap();
            assert_eq!(
                runtime_base(Some(dir.path().into())).unwrap(),
                dir.path().join("nightcap")
            );
            fs::set_permissions(dir.path(), Permissions::from_mode(0o755)).unwrap();
            assert!(runtime_base(Some(dir.path().into())).is_err(), "shared");
            let file = dir.path().join("file");
            fs::write(&file, b"").unwrap();
            assert!(runtime_base(Some(file.into())).is_err(), "not a directory");
            assert!(runtime_base(Some("relative".into())).is_err());
            assert!(runtime_base(Some("".into())).is_err());
            assert!(runtime_base(None).is_err());
        }

        #[test]
        fn a_launch_before_the_first_listens_is_answered() {
            let dir = tempfile::tempdir().unwrap();
            let base = base(&dir);
            let (exit, exit_rx) = mpsc::channel::<()>();
            let (ready_tx, ready) = mpsc::channel();
            let first = {
                let base = base.clone();
                thread::spawn(move || {
                    let Launch::Primary(mut instance) = acquire(&base, TIMEOUT) else {
                        panic!("the first launch must own the instance");
                    };
                    ready_tx.send(()).unwrap();
                    thread::sleep(Duration::from_millis(300));
                    let (shown, activations) = mpsc::channel();
                    instance.on_activate(move |_| shown.send(()).is_ok());
                    let _ = exit_rx.recv();
                    activations.try_iter().count()
                })
            };
            ready.recv_timeout(TIMEOUT).unwrap();
            assert_eq!(outcome(acquire(&base, TIMEOUT)), "shown");
            drop(exit);
            assert_eq!(first.join().unwrap(), 1);
        }

        #[test]
        fn launches_refused_during_quit_start_one_copy_that_shows_for_the_other() {
            let dir = tempfile::tempdir().unwrap();
            let base = base(&dir);
            // A quitting app refuses to show its window.
            let (exit, first) = run_primary(&base, Some(false));
            let launches = [
                later_launch(&base, Duration::from_millis(500)),
                later_launch(&base, Duration::from_millis(500)),
            ];
            thread::sleep(Duration::from_millis(300));
            drop(exit);
            first.join().unwrap();
            let mut outcomes: Vec<_> = launches
                .into_iter()
                .map(|launch| launch.join().unwrap())
                .collect();
            outcomes.sort();
            assert_eq!(outcomes, ["primary", "shown"]);
        }

        #[test]
        fn a_crashed_copy_leaves_nothing_that_blocks_the_next_launch() {
            let dir = tempfile::tempdir().unwrap();
            let base = base(&dir);
            // A crash keeps neither the lock nor a listener, only the socket file.
            drop(UnixListener::bind(base.with_extension("sock")).unwrap());
            fs::write(base.with_extension("lock"), b"").unwrap();
            let started = Instant::now();
            let Launch::Primary(mut instance) = acquire(&base, TIMEOUT) else {
                panic!("a stale socket must not block the next launch");
            };
            assert!(started.elapsed() < Duration::from_secs(1));
            instance.on_activate(|_| true);
            assert_eq!(outcome(acquire(&base, TIMEOUT)), "shown");
        }

        #[test]
        fn a_copy_that_never_answers_is_reported_after_the_timeout() {
            let dir = tempfile::tempdir().unwrap();
            let base = base(&dir);
            let (exit, first) = run_primary(&base, None);
            let started = Instant::now();
            assert_eq!(
                outcome(acquire(&base, Duration::from_millis(400))),
                "failed"
            );
            assert!(started.elapsed() < Duration::from_secs(2));
            drop(exit);
            first.join().unwrap();
        }

        #[test]
        fn a_full_listener_queue_cannot_hold_a_launch_past_its_deadline() {
            use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType};
            let dir = tempfile::tempdir().unwrap();
            let base = base(&dir);
            // Bound but never accepting, with its queue filled.
            let (exit, first) = run_primary(&base, None);
            let socket = base.with_extension("sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let address = SocketAddrUnix::new(&socket).unwrap();
            let mut queued = Vec::new();
            loop {
                let fd = rustix::net::socket_with(
                    AddressFamily::UNIX,
                    SocketType::STREAM,
                    SocketFlags::NONBLOCK | SocketFlags::CLOEXEC,
                    None,
                )
                .unwrap();
                if rustix::net::connect(&fd, &address).is_err() {
                    break;
                }
                queued.push(fd);
                assert!(queued.len() < 100_000, "the queue never filled");
            }
            let (done, outcome_rx) = mpsc::channel();
            let launch = {
                let base = base.clone();
                thread::spawn(move || {
                    let _ = done.send(outcome(acquire(&base, Duration::from_millis(400))));
                })
            };
            assert_eq!(
                outcome_rx.recv_timeout(Duration::from_secs(3)),
                Ok("failed"),
                "the launch waited on the full queue"
            );
            launch.join().unwrap();
            drop((listener, queued, exit));
            first.join().unwrap();
        }

        #[test]
        fn a_copy_that_reads_the_request_but_never_replies_cannot_hold_a_launch() {
            let dir = tempfile::tempdir().unwrap();
            let base = base(&dir);
            let (exit, first) = run_primary(&base, None);
            let listener = UnixListener::bind(base.with_extension("sock")).unwrap();
            let (read_tx, read) = mpsc::channel();
            let peer = thread::spawn(move || {
                let mut held = Vec::new();
                // Accept every attempt, read its request and keep it open unanswered.
                listener.set_nonblocking(true).unwrap();
                let until = Instant::now() + Duration::from_secs(2);
                while Instant::now() < until {
                    if let Ok((mut stream, _)) = listener.accept() {
                        stream.set_nonblocking(false).unwrap();
                        let mut request = [0; 8];
                        if stream.read_exact(&mut request).is_ok() {
                            let _ = read_tx.send(());
                        }
                        held.push(stream);
                    }
                    thread::sleep(Duration::from_millis(10));
                }
            });
            let (done, outcome_rx) = mpsc::channel();
            let launch = {
                let base = base.clone();
                thread::spawn(move || {
                    let _ = done.send(outcome(acquire(&base, Duration::from_millis(400))));
                })
            };
            assert_eq!(
                outcome_rx.recv_timeout(Duration::from_secs(3)),
                Ok("failed"),
                "the launch waited on a reply"
            );
            assert!(read.try_recv().is_ok(), "the request was sent and read");
            launch.join().unwrap();
            peer.join().unwrap();
            drop(exit);
            first.join().unwrap();
        }

        #[test]
        fn an_expired_deadline_never_means_waiting_without_a_timeout() {
            let (mut ours, _theirs) = UnixStream::pair().unwrap();
            let past = Instant::now() - Duration::from_millis(1);
            let error = read_by(&mut ours, &mut [0], past).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::TimedOut);
            let error = write_by(&mut ours, &[0], past).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::TimedOut);
            // An open but silent peer ends the read at the deadline.
            let started = Instant::now();
            let error = read_by(&mut ours, &mut [0], started + Duration::from_millis(100));
            assert!(error.is_err());
            assert!(started.elapsed() < Duration::from_secs(2));
        }

        #[test]
        fn expired_silent_or_malformed_requests_are_refused_without_reaching_the_app() {
            let dir = tempfile::tempdir().unwrap();
            let base = base(&dir);
            let (calls_tx, calls) = mpsc::channel();
            let (exit, exit_rx) = mpsc::channel::<()>();
            let (ready_tx, ready) = mpsc::channel();
            let first = {
                let base = base.clone();
                thread::spawn(move || {
                    let Launch::Primary(mut instance) = acquire(&base, TIMEOUT) else {
                        panic!("the first launch must own the instance");
                    };
                    instance.on_activate(move |_| calls_tx.send(()).is_ok());
                    ready_tx.send(()).unwrap();
                    let _ = exit_rx.recv();
                })
            };
            ready.recv_timeout(TIMEOUT).unwrap();
            assert_eq!(request(&base, in_ms(0).saturating_sub(1)), 0);
            assert_eq!(
                request(&base, in_ms(100)),
                0,
                "less time left than the margin"
            );
            assert_eq!(calls.try_iter().count(), 0);
            assert_eq!(request(&base, in_ms(5000)), 1);
            assert_eq!(calls.try_iter().count(), 1);
            // A far-off deadline is capped rather than trusted.
            assert_eq!(request(&base, u64::MAX), 1);
            assert_eq!(calls.try_iter().count(), 1);
            // A client that disconnects early is dropped.
            drop(UnixStream::connect(base.with_extension("sock")).unwrap());
            assert_eq!(request(&base, in_ms(5000)), 1);
            // A client that stays connected but silent is dropped after its time.
            let silent = UnixStream::connect(base.with_extension("sock")).unwrap();
            let started = Instant::now();
            assert_eq!(request(&base, in_ms(5000)), 1);
            assert!(started.elapsed() < REQUEST_TIMEOUT + Duration::from_secs(1));
            drop(silent);
            // So is one trickling its request a byte at a time: the time limit is for
            // the whole request, not for each read.
            let (trickling, started_trickle) = mpsc::channel();
            let trickle = {
                let socket = base.with_extension("sock");
                thread::spawn(move || {
                    let mut stream = UnixStream::connect(socket).unwrap();
                    for byte in 0..8 {
                        if stream.write_all(&[0]).is_err() {
                            break;
                        }
                        if byte == 0 {
                            trickling.send(()).unwrap();
                        }
                        thread::sleep(Duration::from_millis(600));
                    }
                })
            };
            // Queued behind the trickle, which the listener is already reading.
            started_trickle.recv_timeout(TIMEOUT).unwrap();
            thread::sleep(Duration::from_millis(100));
            let started = Instant::now();
            assert_eq!(request(&base, in_ms(5000)), 1);
            assert!(started.elapsed() < REQUEST_TIMEOUT + Duration::from_secs(1));
            trickle.join().unwrap();
            assert_eq!(calls.try_iter().count(), 3);
            drop(exit);
            first.join().unwrap();
        }
    }
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
mod imp {
    use std::time::Duration;

    use super::Launch;

    /// macOS app bundles are single-instance through LaunchServices.
    pub struct Instance;

    pub(super) const ACTIVATION_TIMEOUT: Duration = Duration::ZERO;

    pub(super) fn acquire(_prefix: &str, _timeout: Duration) -> Launch {
        Launch::Primary(Instance)
    }

    impl Instance {
        pub fn on_activate(&mut self, _activate: impl Fn(Duration) -> bool + Send + 'static) {}
    }
}
