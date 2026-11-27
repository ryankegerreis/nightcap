//! Runs operating-system tools from the timer thread without letting one that
//! hangs stall it. A stuck audio or power tool would otherwise keep the end action
//! from running and leave Cancel and Quit waiting on the engine forever.

use std::{
    fs::File,
    io,
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};

/// Long enough for a slow D-Bus round trip or a busy sound server.
pub(crate) const TIMEOUT: Duration = Duration::from_secs(10);
/// Power requests may wait on logind or the session before they return.
pub(crate) const POWER_TIMEOUT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(10);
/// Far more than any tool here prints. Anything longer is not output to parse.
const OUTPUT_LIMIT: u64 = 1 << 20;

/// The tool started, but whether it did what it was asked is unknown: it was still
/// running at its deadline and was stopped, or waiting for it failed. A power
/// request, for example, may already have been accepted.
#[derive(Debug)]
pub(crate) struct OutcomeUnknown(pub(crate) String);

impl std::fmt::Display for OutcomeUnknown {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for OutcomeUnknown {}

/// Runs `command` to completion and returns its standard output. A nonzero exit is
/// an error carrying its standard error. One still running after `timeout` is
/// killed and reported as [`OutcomeUnknown`]. `Instant` does not advance while the
/// machine sleeps, so a request that returns only after a wake does not count its
/// sleep against the limit.
pub(crate) fn run(command: &mut Command, timeout: Duration) -> Result<String> {
    let finished = execute(command, timeout)?;
    finished.check()?;
    let stdout = finished
        .stdout
        .read()
        .with_context(|| format!("reading {} output", finished.program))?;
    String::from_utf8(stdout).with_context(|| format!("decoding {} output", finished.program))
}

/// Like [`run`], for requests whose exit status is their whole answer, such as power
/// requests: a successful exit counts as done even if its output cannot be read.
pub(crate) fn succeeds(command: &mut Command, timeout: Duration) -> Result<()> {
    execute(command, timeout)?.check()
}

/// What a tool wrote, up to the length the file had when its exit was seen. A
/// descendant still writing may have added to it by then; nothing it adds later is read.
struct Output {
    file: File,
    len: u64,
}

impl Output {
    fn read(&self) -> io::Result<Vec<u8>> {
        if self.len > OUTPUT_LIMIT {
            return Err(io::Error::other(format!(
                "{} bytes is more than a tool's answer",
                self.len
            )));
        }
        let mut bytes = vec![0; self.len as usize];
        // Positional, because a descendant still writing moves the shared offset.
        #[cfg(unix)]
        std::os::unix::fs::FileExt::read_exact_at(&self.file, &mut bytes, 0)?;
        #[cfg(not(unix))]
        {
            use std::io::{Read, Seek, SeekFrom};
            let mut file = &self.file;
            file.seek(SeekFrom::Start(0))?;
            file.read_exact(&mut bytes)?;
        }
        Ok(bytes)
    }
}

struct Finished {
    program: String,
    status: ExitStatus,
    stdout: Output,
    stderr: Output,
}

impl Finished {
    /// A nonzero exit is a definite failure, with whatever the tool said about it.
    fn check(&self) -> Result<()> {
        if self.status.success() {
            return Ok(());
        }
        let stderr = self.stderr.read().unwrap_or_default();
        bail!(
            "{} failed ({}): {}",
            self.program,
            self.status,
            String::from_utf8_lossy(&stderr).trim()
        )
    }
}

fn execute(command: &mut Command, timeout: Duration) -> Result<Finished> {
    let program = command.get_program().to_string_lossy().into_owned();
    // Unlinked files rather than pipes: a descendant that inherits them cannot keep
    // this call waiting for end-of-file after the tool itself has exited.
    let stdout = tempfile::tempfile().context("creating a file for tool output")?;
    let stderr = tempfile::tempfile().context("creating a file for tool errors")?;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(stdout.try_clone().context("sharing the tool output file")?)
        .stderr(stderr.try_clone().context("sharing the tool error file")?)
        .spawn()
        .with_context(|| format!("running {program}"))?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(POLL),
            Ok(None) => {
                abandon(child);
                return Err(OutcomeUnknown(format!(
                    "{program} did not finish within {} s",
                    timeout.as_secs_f32()
                ))
                .into());
            }
            Err(error) => {
                abandon(child);
                return Err(OutcomeUnknown(format!("waiting for {program}: {error}")).into());
            }
        }
    };
    let output = |file: File| {
        let len = file.metadata().map_or(0, |metadata| metadata.len());
        Output { file, len }
    };
    Ok(Finished {
        program,
        status,
        stdout: output(stdout),
        stderr: output(stderr),
    })
}

/// Stops a tool and reaps it on a thread of its own, so one that ignores the kill
/// (or is stuck in the kernel) cannot hold up the caller.
fn abandon(mut child: Child) {
    let _ = child.kill();
    let reaper = thread::Builder::new()
        .name("nightcap-reaper".into())
        .spawn(move || {
            let _ = child.wait();
        });
    if let Err(error) = reaper {
        log::warn!("Could not wait for a stopped tool: {error}");
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }

    #[test]
    fn returns_output_and_reports_failures_with_their_stderr() {
        assert_eq!(run(&mut sh("echo ready"), TIMEOUT).unwrap(), "ready\n");
        for error in [
            run(&mut sh("echo broken >&2; exit 3"), TIMEOUT).unwrap_err(),
            succeeds(&mut sh("echo broken >&2; exit 3"), TIMEOUT).unwrap_err(),
        ] {
            let message = format!("{error:#}");
            assert!(message.contains("broken"), "{message}");
            assert!(!error.is::<OutcomeUnknown>(), "a definite failure");
        }
        assert!(run(&mut Command::new("nightcap-missing-tool"), TIMEOUT).is_err());
    }

    #[test]
    fn a_successful_exit_is_enough_for_a_request_whatever_it_prints() {
        succeeds(&mut sh("exit 0"), TIMEOUT).unwrap();
        // Output `run` would reject as too long, or as not text.
        succeeds(&mut sh("head -c 2000000 /dev/zero"), TIMEOUT).unwrap();
        succeeds(&mut sh("printf '\\377'"), TIMEOUT).unwrap();
        assert!(run(&mut sh("head -c 2000000 /dev/zero"), TIMEOUT).is_err());
        assert!(run(&mut sh("printf '\\377'"), TIMEOUT).is_err());
    }

    #[test]
    fn a_hung_tool_is_killed_at_the_deadline() {
        let started = Instant::now();
        let error = succeeds(&mut sh("exec sleep 30"), Duration::from_millis(200)).unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "waited for the tool"
        );
        assert!(error.is::<OutcomeUnknown>(), "{error:#}");
    }

    #[test]
    fn a_descendant_holding_the_output_open_does_not_hold_up_the_result() {
        // The background sleep inherits stdout and stderr and outlives the tool.
        let started = Instant::now();
        let output = run(&mut sh("sleep 30 & echo ready"), TIMEOUT).unwrap();
        assert_eq!(output, "ready\n");
        assert!(started.elapsed() < Duration::from_secs(5), "waited for EOF");
        // Likewise when the tool itself times out while a descendant lingers.
        let started = Instant::now();
        let error = run(&mut sh("sleep 30 & sleep 30"), Duration::from_millis(200)).unwrap_err();
        assert!(error.is::<OutcomeUnknown>(), "{error:#}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn output_written_after_the_exit_was_seen_is_not_read() {
        use std::io::Write;
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"ready\n").unwrap();
        let output = Output {
            file: file.try_clone().unwrap(),
            len: 6,
        };
        // A descendant keeps writing through the shared offset.
        file.write_all(&[b'x'; 4096]).unwrap();
        assert_eq!(output.read().unwrap(), b"ready\n");
    }

    #[test]
    fn large_output_is_returned_whole() {
        let output = run(&mut sh("head -c 1000000 /dev/zero | tr '\\0' x"), TIMEOUT).unwrap();
        assert_eq!(output.len(), 1_000_000);
    }
}
