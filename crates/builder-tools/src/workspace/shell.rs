//! Bounded subprocess execution and process-tree ownership.

use anyhow::{Context, Result};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::io::{AsyncRead, AsyncReadExt};

const MAX_STREAM_OUTPUT: usize = 12 * 1024;

/// Why a shell command failed, and the bounded output captured before it did.
/// Callers can downcast the `anyhow::Error` from `Workspace::execute` to this
/// type to distinguish a launched command with an uncertain result from a
/// command that completed with a failing exit status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellFailureKind {
    /// The process was launched but did not finish before its deadline. Side
    /// effects may already have happened, so retrying is unsafe by default.
    Timeout,
    /// The process finished and returned a nonzero exit status.
    NonZeroExit,
    /// The process started, but waiting or reading its output failed.
    Incomplete,
}

#[derive(Debug)]
pub struct ShellFailure {
    pub kind: ShellFailureKind,
    pub exit_code: Option<i32>,
    pub output: String,
}

impl std::fmt::Display for ShellFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.kind {
            ShellFailureKind::Timeout => write!(
                f,
                "Command timed out after launch. It may have produced side effects; inspect the workspace before retrying.\n{}",
                self.output
            ),
            ShellFailureKind::NonZeroExit => write!(
                f,
                "Command exited with status {}.\n{}",
                self.exit_code
                    .map_or_else(|| "unknown".into(), |code| code.to_string()),
                self.output
            ),
            ShellFailureKind::Incomplete => write!(
                f,
                "Command outcome is uncertain after launch; inspect before retrying.\n{}",
                self.output
            ),
        }
    }
}

impl std::error::Error for ShellFailure {}

/// Own the process group for exactly the lifetime of this tool. Cancellation,
/// timeout, and normal completion all clean up non-detached descendants.
#[cfg(unix)]
pub(crate) struct ProcessGroup(pub(crate) u32);

#[cfg(unix)]
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(self.0 as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
}

pub(super) async fn run(root: &Path, command: &str, timeout_secs: u64) -> Result<String> {
    #[cfg(windows)]
    let mut process = {
        let mut process = tokio::process::Command::new("cmd");
        process.arg("/C");
        process
    };
    #[cfg(not(windows))]
    let mut process = {
        let mut process = tokio::process::Command::new("sh");
        process.arg("-c");
        process
    };
    #[cfg(unix)]
    process.process_group(0);
    let mut child = process
        .arg(command)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    #[cfg(unix)]
    let group = ProcessGroup(child.id().context("Missing child process ID")?);
    let stdout = child.stdout.take().context("Missing child stdout")?;
    let stderr = child.stderr.take().context("Missing child stderr")?;
    let mut out = Capture::default();
    let mut err = Capture::default();

    let completed = tokio::time::timeout(Duration::from_secs(timeout_secs.clamp(1, 120)), async {
        let (status, out_result, err_result) = tokio::join!(
            child.wait(),
            capture(stdout, &mut out),
            capture(stderr, &mut err)
        );
        out_result?;
        err_result?;
        Ok::<_, anyhow::Error>(status?)
    })
    .await;
    let (status, failure, diagnostic) = match completed {
        Ok(Ok(status)) => (Some(status), None, String::new()),
        failure => {
            let _ = child.start_kill();
            #[cfg(unix)]
            {
                // Kill descendants now so inherited pipe handles close and
                // their already captured output remains available.
                let _ = nix::sys::signal::killpg(
                    nix::unistd::Pid::from_raw(group.0 as i32),
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
            // Do not await inherited pipe handles after the deadline. Detached
            // descendants may keep them open, including on non-Unix systems.
            let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
            match failure {
                Err(_) => (None, Some(ShellFailureKind::Timeout), String::new()),
                Ok(Err(error)) => (
                    None,
                    Some(ShellFailureKind::Incomplete),
                    format!("\nCapture/wait error: {error:#}"),
                ),
                Ok(Ok(_)) => unreachable!(),
            }
        }
    };
    let output = format!(
        "stdout:\n{}\nstderr:\n{}{diagnostic}",
        out.text(),
        err.text()
    );
    if let Some(status) = status {
        if !status.success() {
            return Err(ShellFailure {
                kind: ShellFailureKind::NonZeroExit,
                exit_code: status.code(),
                output,
            }
            .into());
        }
        Ok(format!("exit: {status}\n{output}"))
    } else {
        Err(ShellFailure {
            kind: failure.unwrap_or(ShellFailureKind::Incomplete),
            exit_code: None,
            output,
        }
        .into())
    }
}

#[derive(Default)]
struct Capture {
    head: Vec<u8>,
    tail: std::collections::VecDeque<u8>,
    truncated: bool,
}

impl Capture {
    fn push(&mut self, bytes: &[u8]) {
        let head_limit = MAX_STREAM_OUTPUT / 2;
        let tail_limit = MAX_STREAM_OUTPUT - head_limit;
        for byte in bytes {
            if self.head.len() < head_limit {
                self.head.push(*byte);
            } else {
                if self.tail.len() == tail_limit {
                    self.truncated = true;
                    self.tail.pop_front();
                }
                self.tail.push_back(*byte);
            }
        }
    }

    fn text(&self) -> String {
        let mut saved = self.head.clone();
        if self.truncated {
            saved.extend_from_slice(b"\n[... middle truncated ...]\n");
        }
        saved.extend(&self.tail);
        let text = String::from_utf8_lossy(&saved).into_owned();
        if text.len() <= MAX_STREAM_OUTPUT {
            return text;
        }
        // Invalid UTF-8 expands to replacement characters. Bound the rendered
        // text too, so one binary stream cannot consume the other's budget.
        let marker = "\n[... middle truncated ...]\n";
        let half = (MAX_STREAM_OUTPUT - marker.len()) / 2;
        let mut head_end = half;
        while !text.is_char_boundary(head_end) {
            head_end -= 1;
        }
        let mut tail_start = text.len() - half;
        while !text.is_char_boundary(tail_start) {
            tail_start += 1;
        }
        format!("{}{marker}{}", &text[..head_end], &text[tail_start..])
    }
}

async fn capture(mut stream: impl AsyncRead + Unpin, saved: &mut Capture) -> Result<()> {
    let mut buffer = [0; 8192];
    loop {
        let n = stream.read(&mut buffer).await?;
        if n == 0 {
            break;
        }
        saved.push(&buffer[..n]);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Action, Workspace};

    fn workspace() -> (tempfile::TempDir, Workspace) {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(temp.path()).unwrap();
        (temp, workspace)
    }

    fn shell(command: &str, timeout_secs: u64) -> Action {
        Action::Shell {
            command: command.into(),
            timeout_secs: Some(timeout_secs),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn noisy_stdout_does_not_discard_stderr_tail() {
        let (_temp, workspace) = workspace();
        let error = workspace
            .execute(&shell(
                "head -c 40000 /dev/zero | tr '\\000' x; printf CRITICAL_ERROR >&2; exit 7",
                10,
            ))
            .await
            .unwrap_err();
        let failure = error.downcast_ref::<ShellFailure>().unwrap();
        assert_eq!(failure.kind, ShellFailureKind::NonZeroExit);
        assert_eq!(failure.exit_code, Some(7));
        assert!(failure.output.contains("CRITICAL_ERROR"));
        assert!(failure.output.contains("middle truncated"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_is_typed_and_retains_output_from_both_streams() {
        let (_temp, workspace) = workspace();
        let error = workspace
            // The shell exits promptly, while its background process keeps
            // inherited pipes open. Capture remains inside the deadline.
            .execute(&shell(
                "sleep 4 & printf before-timeout; printf diagnostic >&2",
                1,
            ))
            .await
            .unwrap_err();
        let failure = error.downcast_ref::<ShellFailure>().unwrap();
        assert_eq!(failure.kind, ShellFailureKind::Timeout);
        assert_eq!(failure.exit_code, None);
        assert!(failure.output.contains("before-timeout"));
        assert!(failure.output.contains("diagnostic"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn nonzero_exit_is_reported_as_failure_with_status() {
        let (_temp, workspace) = workspace();
        let error = workspace
            .execute(&shell("printf failed; exit 7", 10))
            .await
            .unwrap_err();
        let failure = error.downcast_ref::<ShellFailure>().unwrap();
        assert_eq!(failure.kind, ShellFailureKind::NonZeroExit);
        assert_eq!(failure.exit_code, Some(7));
        assert!(failure.output.contains("failed"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_after_one_pipe_closes_does_not_repoll_completed_capture() {
        let (_temp, workspace) = workspace();
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            workspace.execute(&shell(
                "printf saved; exec 1>&-; printf diagnostic >&2; sleep 5",
                1,
            )),
        )
        .await
        .expect("shell deadline must remain bounded")
        .unwrap_err();
        let failure = result.downcast_ref::<ShellFailure>().unwrap();
        assert_eq!(failure.kind, ShellFailureKind::Timeout);
        assert!(failure.output.contains("saved"));
        assert!(failure.output.contains("diagnostic"));
    }

    #[test]
    fn binary_capture_is_bounded_after_utf8_expansion() {
        let mut capture = Capture::default();
        capture.push(&vec![255; MAX_STREAM_OUTPUT * 3]);
        let text = capture.text();
        assert!(text.len() <= MAX_STREAM_OUTPUT);
        assert!(text.contains("middle truncated"));
        let mut small = Capture::default();
        small.push(&vec![b'x'; MAX_STREAM_OUTPUT - 1]);
        assert_eq!(small.text().len(), MAX_STREAM_OUTPUT - 1);
        assert!(!small.text().contains("truncated"));
    }
}
