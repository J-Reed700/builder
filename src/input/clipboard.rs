//! Read clipboard text without streaming it through the terminal's paste path.
use std::io;
#[cfg(any(target_os = "macos", all(test, unix)))]
use std::{process::Stdio, time::Duration};
#[cfg(any(target_os = "macos", all(test, unix)))]
use tokio::{io::AsyncReadExt, process::Command};

pub fn read(limit: usize) -> io::Result<String> {
    #[cfg(target_os = "macos")]
    {
        let runtime = tokio::runtime::Handle::try_current().map_err(io::Error::other)?;
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        let task = runtime.spawn(async move {
            let result = read_command(
                Command::new("/usr/bin/pbpaste"),
                limit,
                Duration::from_secs(2),
            )
            .await;
            let _ = send.send(result);
        });
        // This synchronous composer runs on the multi-thread runtime's main
        // thread. The subprocess runs on a worker; queued terminal input stays
        // in order until the clipboard insertion finishes.
        let result = receive
            .recv_timeout(Duration::from_secs(3))
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "Clipboard read timed out"));
        task.abort();
        result?
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = limit;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Direct clipboard paste is macOS-only; use your terminal's paste shortcut",
        ))
    }
}

#[cfg(any(target_os = "macos", all(test, unix)))]
async fn read_command(mut command: Command, limit: usize, timeout: Duration) -> io::Result<String> {
    tokio::time::timeout(timeout, async {
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let mut bytes = Vec::new();
        child
            .stdout
            .take()
            .expect("piped stdout")
            .take(limit as u64 + 1)
            .read_to_end(&mut bytes)
            .await?;
        if bytes.len() > limit {
            return Err(io::Error::other(
                "Clipboard exceeds the remaining 4 MiB draft limit",
            ));
        }
        if !child.wait().await?.success() {
            return Err(io::Error::other("Could not read clipboard text"));
        }
        String::from_utf8(bytes).map_err(|_| io::Error::other("Clipboard text is not valid UTF-8"))
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "Clipboard read timed out"))?
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn preserves_text_and_rejects_oversized_clipboard_in_full() {
        let text = "  /exit\r\n🦀\n".repeat(65536);
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), &text).unwrap();
        let command = || {
            let mut command = Command::new("/bin/cat");
            command.arg(file.path());
            command
        };
        assert_eq!(
            read_command(command(), text.len(), Duration::from_secs(1))
                .await
                .unwrap(),
            text
        );
        assert!(
            read_command(command(), 4, Duration::from_secs(1))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn clipboard_failure_and_hang_are_bounded() {
        assert!(
            read_command(Command::new("/usr/bin/false"), 100, Duration::from_secs(1))
                .await
                .is_err()
        );
        let mut command = Command::new("/bin/sleep");
        command.arg("10");
        let error = read_command(command, 100, Duration::from_millis(30))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }
}
