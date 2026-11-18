//! Update notice: about once a day, asks GitHub which release is the latest.
//!
//! The check runs the operating system's `curl` on a background thread and reads
//! the redirect from `/releases/latest` to `/releases/tag/vX.Y.Z`, so Nightcap
//! needs no HTTP or TLS crate. Failures are logged and otherwise ignored, except
//! that a failed manual check is reported in Settings.

use std::{
    cmp::Ordering,
    process::{Command, Stdio},
    sync::{
        Arc, Mutex, MutexGuard,
        mpsc::{self, RecvTimeoutError},
    },
    time::{Duration, SystemTime},
};

use anyhow::{Context as _, Result, bail};
use semver::Version;

pub const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");

/// The network may not be up yet when Nightcap starts at login.
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(10);
const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const RETRY_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// Waits are cut into hourly pieces and compared with the wall clock, because a
/// sleeping machine may not advance a monotonic timeout.
const MAX_WAIT: Duration = Duration::from_secs(60 * 60);
const CURL_MAX_TIME_SECS: &str = "20";

/// A published release newer than this build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    pub page_url: String,
}

impl Release {
    /// This platform's asset from the tag itself, so the file matches the version
    /// shown. Platforms without a published build get the release page.
    pub fn download_url(&self) -> String {
        match asset_for(std::env::consts::OS, std::env::consts::ARCH) {
            Some(asset) => format!("{REPOSITORY}/releases/download/v{}/{asset}", self.version),
            None => self.page_url.clone(),
        }
    }
}

fn asset_for(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("macos", _) => Some("Nightcap-macOS.dmg"),
        // Arm builds are not published (and Windows 10 on Arm cannot emulate x64).
        ("windows", "x86_64") => Some("Nightcap-Windows-x64.zip"),
        ("linux", "x86_64") => Some("Nightcap-Linux-x86_64.tar.gz"),
        _ => None,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateStatus {
    /// The version this copy reports, shown in Settings.
    pub current: Version,
    pub checking: bool,
    /// Set by a successful check that found a newer release; kept through later failures.
    pub available: Option<Release>,
    /// A check has succeeded since launch.
    pub checked: bool,
    /// The latest manual check failed and nothing has succeeded since.
    pub failed: bool,
}

impl UpdateStatus {
    /// Nothing checked yet.
    pub fn new(current: Version) -> Self {
        Self {
            current,
            checking: false,
            available: None,
            checked: false,
            failed: false,
        }
    }

    /// One line for Settings, under the installed version.
    pub fn caption(&self) -> String {
        if self.checking {
            "Checking…".into()
        } else if self.failed {
            // Download stays offered for a release found earlier.
            "Couldn’t check for updates".into()
        } else if let Some(release) = &self.available {
            format!("Version {} is available", release.version)
        } else if self.checked {
            "Up to date".into()
        } else {
            "Not checked yet".into()
        }
    }
}

/// The version this build reports. Debug builds accept `NIGHTCAP_CURRENT_VERSION`
/// to preview the update notice against real releases.
pub fn current_version() -> Version {
    let built = Version::parse(env!("CARGO_PKG_VERSION")).expect("Cargo versions are semver");
    #[cfg(debug_assertions)]
    if let Ok(value) = std::env::var("NIGHTCAP_CURRENT_VERSION") {
        match Version::parse(value.trim()) {
            Ok(version) => {
                log::warn!("Reporting version {version} for update checks");
                return version;
            }
            Err(_) => log::warn!("Invalid NIGHTCAP_CURRENT_VERSION={value:?}; using {built}"),
        }
    }
    built
}

/// Opens a link in the default browser. Unlike `egui::Context::open_url`, this
/// works from the tray while every Nightcap window is hidden.
pub fn open_in_browser(url: &str) {
    if let Err(error) = webbrowser::open(url) {
        log::warn!("Could not open {url}: {error}");
    }
}

/// Reads the redirect GitHub sends for `/releases/latest`: the last response in
/// `headers` must be a 3xx whose `Location` is one of this repository's tags.
pub fn parse_latest(headers: &str) -> Option<Release> {
    let normalized = headers.replace("\r\n", "\n");
    // Proxies and interim responses add earlier blocks; the last one is GitHub's.
    let block = normalized
        .split("\n\n")
        .filter(|block| !block.trim().is_empty())
        .last()?;
    let mut lines = block.trim_start().lines();
    let status_line = lines.next()?;
    if !status_line.starts_with("HTTP/") {
        return None;
    }
    let status: u16 = status_line.split_whitespace().nth(1)?.parse().ok()?;
    if !(300..400).contains(&status) {
        return None;
    }
    let location = lines.find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("location")
            .then(|| value.trim())
    })?;
    let tag = location.strip_prefix(&format!("{REPOSITORY}/releases/tag/v"))?;
    let version = Version::parse(tag).ok()?;
    Some(Release {
        version,
        page_url: location.to_owned(),
    })
}

/// Semver precedence, which ignores build metadata: `0.1.2+abc` is not newer than `0.1.2`.
pub fn newer(current: &Version, latest: &Version) -> bool {
    latest.cmp_precedence(current) == Ordering::Greater
}

/// When the next automatic check is due after one finishes at `now`.
fn next_due(now: SystemTime, succeeded: bool) -> SystemTime {
    now + if succeeded {
        CHECK_INTERVAL
    } else {
        RETRY_INTERVAL
    }
}

/// How long to wait for a request before looking at the clock again. A deadline
/// that passed while the machine slept is due now.
fn wait_for(now: SystemTime, due: Option<SystemTime>) -> Duration {
    due.map_or(MAX_WAIT, |due| {
        due.duration_since(now).unwrap_or(Duration::ZERO)
    })
    .min(MAX_WAIT)
}

enum Request {
    SetEnabled(bool),
    CheckNow,
}

type Fetch = Box<dyn Fn() -> Result<String> + Send>;
type Wake = Arc<dyn Fn() + Send + Sync>;

/// State shared by the handle and the worker, under one lock so a check is
/// reserved exactly once.
struct Shared {
    status: UpdateStatus,
    /// A manual check is waiting for, or riding on, the next result: its failure is shown.
    manual: bool,
}

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

/// Handle to the update-check thread. Dropping it lets the thread end at its next
/// wake; it is never joined, so quitting does not wait for curl.
pub struct UpdateChecker {
    shared: Arc<Mutex<Shared>>,
    requests: mpsc::Sender<Request>,
    enabled: bool,
}

impl UpdateChecker {
    /// `wake` runs on the checker thread after each status change.
    pub fn spawn(enabled: bool, wake: Wake) -> Self {
        Self::spawn_with(
            enabled,
            current_version(),
            FIRST_CHECK_DELAY,
            Box::new(fetch_headers),
            wake,
        )
    }

    fn spawn_with(
        enabled: bool,
        current: Version,
        first_delay: Duration,
        fetch: Fetch,
        wake: Wake,
    ) -> Self {
        let shared = Arc::new(Mutex::new(Shared {
            status: UpdateStatus::new(current),
            manual: false,
        }));
        let (requests, receiver) = mpsc::channel();
        let worker = Worker {
            shared: Arc::clone(&shared),
            fetch,
            wake,
        };
        if let Err(error) = std::thread::Builder::new()
            .name("nightcap-updates".into())
            .spawn(move || worker.run(receiver, enabled, first_delay))
        {
            log::warn!("Update checks unavailable: {error}");
        }
        Self {
            shared,
            requests,
            enabled,
        }
    }

    pub fn status(&self) -> UpdateStatus {
        lock(&self.shared).status.clone()
    }

    /// Turning checks on checks right away. Repeating the current value does nothing.
    pub fn set_enabled(&mut self, enabled: bool) {
        if self.enabled != enabled {
            self.enabled = enabled;
            let _ = self.requests.send(Request::SetEnabled(enabled));
        }
    }

    /// Checks now, whether or not automatic checks are on, and reports a failure.
    /// A check already under way answers this request too.
    pub fn check_now(&self) {
        let mut shared = lock(&self.shared);
        shared.manual = true;
        if shared.status.checking {
            return;
        }
        // Shown at once, so a second click cannot queue another check.
        shared.status.checking = true;
        if self.requests.send(Request::CheckNow).is_err() {
            log::info!("Update check failed: the update thread is not running");
            shared.manual = false;
            shared.status.checking = false;
            shared.status.failed = true;
        }
    }
}

struct Worker {
    shared: Arc<Mutex<Shared>>,
    fetch: Fetch,
    wake: Wake,
}

impl Worker {
    fn run(self, requests: mpsc::Receiver<Request>, mut enabled: bool, first_delay: Duration) {
        let mut due = enabled.then(|| SystemTime::now() + first_delay);
        loop {
            match requests.recv_timeout(wait_for(SystemTime::now(), due)) {
                Ok(Request::SetEnabled(on)) => {
                    if on != enabled {
                        enabled = on;
                        due = on.then(SystemTime::now);
                    }
                    continue;
                }
                // An automatic check that started just before the click already answered it.
                Ok(Request::CheckNow) if !lock(&self.shared).manual => continue,
                Ok(Request::CheckNow) => {}
                Err(RecvTimeoutError::Timeout) => {
                    if !due.is_some_and(|due| SystemTime::now() >= due) {
                        continue;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
            let succeeded = self.check();
            due = enabled.then(|| next_due(SystemTime::now(), succeeded));
        }
    }

    fn check(&self) -> bool {
        let current = {
            let mut shared = lock(&self.shared);
            shared.status.checking = true;
            shared.status.current.clone()
        };
        (self.wake)();
        let result = (self.fetch)().and_then(|headers| {
            parse_latest(&headers).context("GitHub did not redirect to a release tag")
        });
        let succeeded = result.is_ok();
        {
            let mut shared = lock(&self.shared);
            // Clicks during the request share its result.
            let manual = std::mem::take(&mut shared.manual);
            let status = &mut shared.status;
            status.checking = false;
            match result {
                Ok(release) => {
                    log::info!("Latest Nightcap release is {}", release.version);
                    status.checked = true;
                    status.failed = false;
                    status.available = newer(&current, &release.version).then_some(release);
                }
                Err(error) => {
                    log::info!("Update check failed: {error:#}");
                    status.failed |= manual;
                }
            }
        }
        (self.wake)();
        succeeded
    }
}

/// Response headers for `/releases/latest`, without following the redirect.
fn fetch_headers() -> Result<String> {
    let url = format!("{REPOSITORY}/releases/latest");
    let user_agent = format!("Nightcap/{} (update check)", env!("CARGO_PKG_VERSION"));
    let mut command = Command::new(curl_program());
    // `--disable` must come first: it skips the user's .curlrc, which could add
    // options such as --location. Every flag here exists in curl 7.55, the
    // version Windows 10 1803 ships.
    command
        .args([
            "--disable",
            "--silent",
            "--show-error",
            "--head",
            "--max-time",
            CURL_MAX_TIME_SECS,
            "--proto",
            "=https",
            "--user-agent",
            &user_agent,
            &url,
        ])
        .stdin(Stdio::null());
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt as _;
        // Release builds have no console; curl would otherwise open one.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let output = command.output().context("running curl")?;
    if !output.status.success() {
        bail!(
            "curl exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The system's own curl, by absolute path where it has a fixed one, so a
/// different `curl` earlier on PATH is not run instead.
fn curl_program() -> std::path::PathBuf {
    #[cfg(target_os = "macos")]
    {
        "/usr/bin/curl".into()
    }
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("SystemRoot")
            .map_or_else(|| r"C:\Windows".into(), std::path::PathBuf::from)
            .join(r"System32\curl.exe")
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        "curl".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    fn redirect(location: &str) -> String {
        format!("HTTP/2 302 \r\ncontent-type: text/html\r\nlocation: {location}\r\n\r\n")
    }

    #[test]
    fn reads_the_tag_from_githubs_redirect() {
        let url = format!("{REPOSITORY}/releases/tag/v0.2.0");
        let release = parse_latest(&redirect(&url)).unwrap();
        assert_eq!(release.version, version("0.2.0"));
        assert_eq!(release.page_url, url);
        // LF-only output, a mixed-case header name and an HTTP/1.1 status line.
        let headers = format!("HTTP/1.1 301 Moved Permanently\nLoCaTiOn:   {url}  \n\n");
        assert_eq!(parse_latest(&headers).unwrap().version, version("0.2.0"));
    }

    #[test]
    fn uses_the_last_response_block() {
        let tag = format!("{REPOSITORY}/releases/tag/v1.4.0");
        let proxied = format!(
            "HTTP/1.1 200 Connection established\r\n\r\n{}",
            redirect(&tag)
        );
        assert_eq!(parse_latest(&proxied).unwrap().version, version("1.4.0"));
        // A redirect followed by its target page is not a redirect.
        let followed = format!("{}HTTP/2 200 \r\ncontent-length: 0\r\n\r\n", redirect(&tag));
        assert_eq!(parse_latest(&followed), None);
    }

    #[test]
    fn rejects_anything_but_a_release_tag_of_this_repository() {
        let tag = format!("{REPOSITORY}/releases/tag/v0.2.0");
        for headers in [
            String::new(),
            "garbage".to_owned(),
            format!("HTTP/2 200 \r\nlocation: {tag}\r\n\r\n"),
            format!("HTTP/2 404 \r\nlocation: {tag}\r\n\r\n"),
            "HTTP/2 302 \r\ncontent-length: 0\r\n\r\n".to_owned(),
            // No releases yet: GitHub redirects to the release list.
            redirect(&format!("{REPOSITORY}/releases")),
            redirect("https://example.com/ryankegerreis/nightcap/releases/tag/v0.2.0"),
            redirect(&format!("{REPOSITORY}-fork/releases/tag/v0.2.0")),
            redirect(&format!("{REPOSITORY}/releases/tag/0.2.0")),
            redirect(&format!("{REPOSITORY}/releases/tag/vnext")),
            redirect(&format!("{REPOSITORY}/releases/tag/v0.2")),
        ] {
            assert_eq!(parse_latest(&headers), None, "{headers:?}");
        }
    }

    #[test]
    fn only_strictly_newer_versions_count() {
        let current = version("0.1.2");
        assert!(!newer(&current, &version("0.1.2")));
        assert!(!newer(&current, &version("0.1.1")));
        assert!(newer(&current, &version("0.1.3")));
        assert!(newer(&current, &version("0.2.0")));
        assert!(newer(&current, &version("0.2.0-beta.1")));
        assert!(!newer(&version("0.2.0"), &version("0.2.0-beta.1")));
        assert!(newer(&version("0.9.0"), &version("0.10.0")));
        // Build metadata carries no precedence.
        assert!(!newer(&current, &version("0.1.2+build.5")));
        assert!(!newer(&version("0.1.2+a"), &version("0.1.2+b")));
    }

    #[test]
    fn downloads_this_platforms_asset_from_the_tag() {
        for (os, arch, asset) in [
            ("macos", "aarch64", Some("Nightcap-macOS.dmg")),
            ("macos", "x86_64", Some("Nightcap-macOS.dmg")),
            ("windows", "x86_64", Some("Nightcap-Windows-x64.zip")),
            ("windows", "aarch64", None),
            ("linux", "x86_64", Some("Nightcap-Linux-x86_64.tar.gz")),
            ("linux", "aarch64", None),
            ("freebsd", "x86_64", None),
        ] {
            assert_eq!(asset_for(os, arch), asset, "{os} {arch}");
        }
        let release = Release {
            version: version("0.2.0"),
            page_url: format!("{REPOSITORY}/releases/tag/v0.2.0"),
        };
        let url = release.download_url();
        match asset_for(std::env::consts::OS, std::env::consts::ARCH) {
            Some(asset) => assert_eq!(
                url,
                format!(
                    "https://github.com/ryankegerreis/nightcap/releases/download/v0.2.0/{asset}"
                )
            ),
            None => assert_eq!(url, release.page_url),
        }
    }

    #[test]
    fn schedules_daily_checks_hourly_retries_and_overdue_checks_at_once() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        assert_eq!(next_due(now, true), now + Duration::from_secs(86_400));
        assert_eq!(next_due(now, false), now + Duration::from_secs(3_600));
        assert_eq!(wait_for(now, None), MAX_WAIT);
        assert_eq!(
            wait_for(now, Some(now + Duration::from_secs(10))),
            Duration::from_secs(10)
        );
        // A day away: wake hourly to compare against the wall clock.
        assert_eq!(wait_for(now, Some(next_due(now, true))), MAX_WAIT);
        // Slept past the deadline.
        assert_eq!(
            wait_for(now, Some(now - Duration::from_secs(5))),
            Duration::ZERO
        );
    }

    #[test]
    fn captions_follow_the_check_state() {
        let mut status = UpdateStatus::new(version("0.1.2"));
        assert_eq!(status.caption(), "Not checked yet");
        status.failed = true;
        assert_eq!(status.caption(), "Couldn’t check for updates");
        status.failed = false;
        status.checked = true;
        assert_eq!(status.caption(), "Up to date");
        status.available = Some(Release {
            version: version("0.2.0"),
            page_url: String::new(),
        });
        assert_eq!(status.caption(), "Version 0.2.0 is available");
        status.failed = true;
        assert_eq!(status.caption(), "Couldn’t check for updates");
        status.failed = false;
        status.checking = true;
        assert_eq!(status.caption(), "Checking…");
    }

    /// Runs a checker against canned fetch results and waits for each status change.
    struct Harness {
        checker: UpdateChecker,
        woken: mpsc::Receiver<()>,
        responses: mpsc::Sender<Result<String>>,
    }

    impl Harness {
        fn new(enabled: bool, first_delay: Duration) -> Self {
            let (responses, pending) = mpsc::channel::<Result<String>>();
            let pending = Mutex::new(pending);
            let (wake, woken) = mpsc::channel();
            let checker = UpdateChecker::spawn_with(
                enabled,
                version("0.1.2"),
                first_delay,
                Box::new(move || {
                    pending
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(5))
                        .context("no canned response")?
                }),
                Arc::new(move || {
                    let _ = wake.send(());
                }),
            );
            Self {
                checker,
                woken,
                responses,
            }
        }

        /// One check: the start and finish wakes, with `response` returned between them.
        fn finish(&self, response: Result<String>) -> UpdateStatus {
            self.woken.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(self.checker.status().checking);
            self.responses.send(response).unwrap();
            self.woken.recv_timeout(Duration::from_secs(5)).unwrap();
            let status = self.checker.status();
            assert!(!status.checking);
            status
        }

        fn quiet(&self) {
            assert!(self.woken.recv_timeout(Duration::from_millis(200)).is_err());
        }
    }

    fn tag(version: &str) -> Result<String> {
        Ok(redirect(&format!("{REPOSITORY}/releases/tag/v{version}")))
    }

    #[test]
    fn manual_checks_report_results_and_failures() {
        let harness = Harness::new(false, Duration::ZERO);
        harness.quiet();
        harness.checker.check_now();
        assert_eq!(harness.checker.status().caption(), "Checking…");
        let status = harness.finish(Err(anyhow::anyhow!("offline")));
        assert!(status.failed);
        assert_eq!(status.available, None);
        harness.checker.check_now();
        let status = harness.finish(tag("0.3.0"));
        assert!(!status.failed && status.checked);
        assert_eq!(status.available.unwrap().version, version("0.3.0"));
        harness.checker.check_now();
        let status = harness.finish(tag("0.1.2"));
        assert_eq!(status.caption(), "Up to date");
        // Disabled: no automatic retry after a manual check.
        harness.quiet();
    }

    #[test]
    fn automatic_checks_start_on_launch_or_when_enabled_and_fail_silently() {
        let harness = Harness::new(true, Duration::ZERO);
        let status = harness.finish(Err(anyhow::anyhow!("offline")));
        assert!(!status.failed && !status.checked);
        assert_eq!(status.caption(), "Not checked yet");
        harness.quiet();

        let mut harness = Harness::new(false, Duration::ZERO);
        harness.checker.set_enabled(false);
        harness.quiet();
        harness.checker.set_enabled(true);
        let status = harness.finish(tag("0.2.0"));
        assert_eq!(status.available.unwrap().version, version("0.2.0"));
        // A later manual failure is shown and keeps the known release.
        harness.checker.check_now();
        let status = harness.finish(Err(anyhow::anyhow!("offline")));
        assert!(status.failed);
        assert_eq!(status.caption(), "Couldn’t check for updates");
        assert_eq!(status.available.unwrap().version, version("0.2.0"));
    }

    #[test]
    fn a_click_during_an_automatic_check_shares_its_result() {
        let harness = Harness::new(true, Duration::ZERO);
        // The automatic check is waiting on its response.
        harness.woken.recv_timeout(Duration::from_secs(5)).unwrap();
        harness.checker.check_now();
        harness
            .responses
            .send(Err(anyhow::anyhow!("offline")))
            .unwrap();
        harness.woken.recv_timeout(Duration::from_secs(5)).unwrap();
        let status = harness.checker.status();
        assert!(!status.checking && status.failed);
        // No second request.
        harness.quiet();
    }

    #[test]
    fn a_click_already_answered_by_an_automatic_check_is_skipped() {
        let harness = Harness::new(false, Duration::ZERO);
        // The click's request arrives after a check that started first took it.
        lock(&harness.checker.shared).manual = false;
        harness.checker.requests.send(Request::CheckNow).unwrap();
        harness.quiet();
        // A real click still runs.
        harness.checker.check_now();
        assert!(harness.finish(tag("0.1.2")).checked);
    }

    #[test]
    fn check_now_without_a_worker_reports_a_failure() {
        let (requests, receiver) = mpsc::channel();
        drop(receiver);
        let checker = UpdateChecker {
            shared: Arc::new(Mutex::new(Shared {
                status: UpdateStatus::new(version("0.1.2")),
                manual: false,
            })),
            requests,
            enabled: true,
        };
        checker.check_now();
        let status = checker.status();
        assert!(!status.checking);
        assert_eq!(status.caption(), "Couldn’t check for updates");
    }

    #[test]
    fn a_delayed_first_check_waits() {
        let harness = Harness::new(true, Duration::from_secs(60));
        harness.quiet();
        assert_eq!(harness.checker.status().caption(), "Not checked yet");
    }

    /// Network check against GitHub with the real curl:
    /// `cargo test --lib updates -- --ignored`.
    #[test]
    #[ignore = "contacts github.com"]
    fn github_redirects_to_a_published_release() {
        let headers = fetch_headers().unwrap();
        let release = parse_latest(&headers).unwrap_or_else(|| panic!("{headers}"));
        assert!(release.version >= version("0.1.2"), "{}", release.version);
        println!("latest: {} ({})", release.version, release.download_url());
    }
}
