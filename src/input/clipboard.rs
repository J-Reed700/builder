//! Read clipboard text or images without streaming them through the
//! terminal's paste path.
use std::io;
#[cfg(any(target_os = "macos", all(test, unix)))]
use std::{process::Stdio, time::Duration};
#[cfg(any(target_os = "macos", all(test, unix)))]
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};

use builder_core::protocol::Image;

/// A pasted image is capped at the same size as a text draft.
#[cfg(any(target_os = "macos", all(test, unix)))]
const MAX_IMAGE_BYTES: usize = 4 * 1024 * 1024;

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

/// Read the pasteboard's image as base64 with its MIME type. `pbpaste` cannot
/// read images, so a short JavaScript for Automation helper asks AppKit for the
/// PNG/JPEG/GIF data or converts the TIFF that screenshots produce. JXA ships
/// with every macOS and starts in well under 100ms; `swift` needs the Command
/// Line Tools and can take seconds to compile a script on a cold cache.
pub fn read_image() -> io::Result<Image> {
    #[cfg(target_os = "macos")]
    {
        let runtime = tokio::runtime::Handle::try_current().map_err(io::Error::other)?;
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        let task = runtime.spawn(async move {
            let mut command = Command::new("/usr/bin/osascript");
            command.args(["-l", "JavaScript", "-"]);
            let result = read_image_command(command, IMAGE_HELPER, Duration::from_secs(3)).await;
            let _ = send.send(result);
        });
        let result = receive
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "Clipboard read timed out"));
        task.abort();
        result?
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Image paste is macOS-only; use /attach <image> instead",
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

/// JXA helper that writes `mime\nbase64` for the pasteboard's image,
/// converting the TIFF that macOS screenshots produce to PNG. Exits 1 when the
/// pasteboard holds no image.
#[cfg(target_os = "macos")]
const IMAGE_HELPER: &str = r#"
ObjC.import("AppKit");
ObjC.import("stdlib");
function run() {
  var pb = $.NSPasteboard.generalPasteboard;
  var kinds = [["public.png", "image/png"], ["public.jpeg", "image/jpeg"], ["com.compuserve.gif", "image/gif"]];
  var data = null, mime = null;
  for (var i = 0; i < kinds.length && data === null; i++) {
    var d = pb.dataForType(kinds[i][0]);
    if (!d.isNil()) { data = d; mime = kinds[i][1]; }
  }
  if (data === null) {
    var tiff = pb.dataForType("public.tiff");
    if (!tiff.isNil()) {
      var rep = $.NSBitmapImageRep.imageRepWithData(tiff);
      if (!rep.isNil()) {
        var png = rep.representationUsingTypeProperties($.NSBitmapImageFileTypePNG, $());
        if (!png.isNil()) { data = png; mime = "image/png"; }
      }
    }
  }
  if (data === null) { $.exit(1); }
  var out = $.NSString.alloc.initWithString(mime + "\n")
    .stringByAppendingString(data.base64EncodedStringWithOptions(0));
  $.NSFileHandle.fileHandleWithStandardOutput.writeData(out.dataUsingEncoding($.NSUTF8StringEncoding));
}
"#;

/// Largest helper output accepted: the base64 of a maximal image plus a MIME
/// line. Anything longer is rejected without waiting for the helper.
#[cfg(any(target_os = "macos", all(test, unix)))]
const MAX_IMAGE_OUTPUT: usize = MAX_IMAGE_BYTES.div_ceil(3) * 4 + 64;

#[cfg(any(target_os = "macos", all(test, unix)))]
async fn read_image_command(
    mut command: Command,
    script: &str,
    timeout: Duration,
) -> io::Result<Image> {
    const NO_IMAGE: &str = "Clipboard has no image (screenshots and copied images do)";
    tokio::time::timeout(timeout, async {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        // A helper that exits before reading its script (as one with nothing
        // to do may) closes the pipe; its exit status reports the outcome.
        let _ = stdin.write_all(script.as_bytes()).await;
        drop(stdin);
        let mut bytes = Vec::new();
        child
            .stdout
            .take()
            .expect("piped stdout")
            .take(MAX_IMAGE_OUTPUT as u64 + 1)
            .read_to_end(&mut bytes)
            .await?;
        // Fail fast: an oversized helper would otherwise block on a full pipe
        // until the timeout and be misreported as slow. Dropping the child
        // kills it.
        if bytes.len() > MAX_IMAGE_OUTPUT {
            return Err(io::Error::other("Image exceeds the 4 MiB paste limit"));
        }
        if !child.wait().await?.success() {
            return Err(io::Error::other(NO_IMAGE));
        }
        let newline = bytes
            .iter()
            .position(|&b| b == b'\n')
            .ok_or_else(|| io::Error::other("Clipboard image read failed"))?;
        let mut data = bytes.split_off(newline + 1);
        bytes.truncate(newline);
        while data.last().is_some_and(u8::is_ascii_whitespace) {
            data.pop();
        }
        if bytes.is_empty() || data.is_empty() {
            return Err(io::Error::other(NO_IMAGE));
        }
        if base64_decoded_len(&data) > MAX_IMAGE_BYTES {
            return Err(io::Error::other("Image exceeds the 4 MiB paste limit"));
        }
        let invalid = |_| io::Error::other("Clipboard image read failed");
        Ok(Image {
            mime: String::from_utf8(bytes).map_err(invalid)?,
            data: String::from_utf8(data).map_err(invalid)?,
        })
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "Clipboard read timed out"))?
}

/// Decoded size of padded standard base64.
#[cfg(any(target_os = "macos", all(test, unix)))]
fn base64_decoded_len(data: &[u8]) -> usize {
    let padding = data.iter().rev().take(2).filter(|&&b| b == b'=').count();
    (data.len() / 4 * 3).saturating_sub(padding)
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

    fn sh(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }

    #[tokio::test]
    async fn image_helper_reports_no_image_without_hanging() {
        let error = read_image_command(sh("exit 1"), "", Duration::from_secs(2))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("no image"), "{error}");
    }

    #[tokio::test]
    async fn image_helper_output_is_split_into_mime_and_payload() {
        // The script arrives on stdin, exactly as osascript receives it.
        let image = read_image_command(
            sh("read -r mime; printf '%s\\nAAAA\\n' \"$mime\""),
            "image/png\n",
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        assert_eq!(image.mime, "image/png");
        assert_eq!(image.data, "AAAA");
    }

    #[tokio::test]
    async fn oversized_images_fail_fast_instead_of_timing_out() {
        // The helper would stream forever; the cap must cut it off at once.
        let started = std::time::Instant::now();
        let error = read_image_command(
            sh("printf 'image/png\\n'; exec yes AAAA"),
            "",
            Duration::from_secs(20),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("4 MiB"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn base64_length_accounts_for_padding() {
        assert_eq!(base64_decoded_len(b"aGVsbG8="), 5);
        assert_eq!(base64_decoded_len(b"aGVsbA=="), 4);
        assert_eq!(base64_decoded_len(b"aGVs"), 3);
        assert_eq!(base64_decoded_len(b""), 0);
    }

    /// The real helper against a private pasteboard, so the user's clipboard
    /// is never touched.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn jxa_helper_reads_png_and_converts_screenshot_tiff() {
        let name = format!("builder-test-{}", std::process::id());
        let script = IMAGE_HELPER.replace(
            "$.NSPasteboard.generalPasteboard",
            &format!("$.NSPasteboard.pasteboardWithName({name:?})"),
        );
        let osascript = || {
            let mut command = Command::new("/usr/bin/osascript");
            command.args(["-l", "JavaScript", "-"]);
            command
        };
        let set = |kind: &str, path: &std::path::Path| {
            format!(
                "ObjC.import('AppKit'); var p = $.NSPasteboard.pasteboardWithName({name:?}); p.clearContents; {}",
                if kind.is_empty() {
                    String::new()
                } else {
                    format!(
                        "p.setDataForType($.NSData.dataWithContentsOfFile({:?}), {kind:?});",
                        path.display()
                    )
                }
            )
        };
        let run = |js: String| {
            let status = std::process::Command::new("/usr/bin/osascript")
                .args(["-l", "JavaScript", "-e", &js])
                .stdout(Stdio::null())
                .status()
                .unwrap();
            assert!(status.success());
        };
        // A 1x1 RGB PNG.
        let png: &[u8] = &[
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00,
            0x00, 0x90, 0x77, 0x53, 0xde, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9c, 0x63, 0xf8, 0xcf, 0xc0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0xc9, 0xfe, 0x92,
            0xef, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
        ];
        let dir = tempfile::tempdir().unwrap();
        let png_path = dir.path().join("one.png");
        std::fs::write(&png_path, png).unwrap();

        run(set("", &png_path));
        let empty = read_image_command(osascript(), &script, Duration::from_secs(10)).await;
        assert!(empty.unwrap_err().to_string().contains("no image"));

        run(set("public.png", &png_path));
        let image = read_image_command(osascript(), &script, Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(image.mime, "image/png");
        assert_eq!(
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &image.data)
                .unwrap(),
            png
        );

        // macOS screenshots land on the pasteboard as TIFF.
        let tiff_path = dir.path().join("one.tiff");
        let converted = std::process::Command::new("/usr/bin/sips")
            .args(["-s", "format", "tiff"])
            .arg(&png_path)
            .arg("--out")
            .arg(&tiff_path)
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(converted.success());
        run(set("public.tiff", &tiff_path));
        let image = read_image_command(osascript(), &script, Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(image.mime, "image/png");
        let decoded =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &image.data)
                .unwrap();
        assert!(decoded.starts_with(b"\x89PNG"));

        run(format!(
            "ObjC.import('AppKit'); $.NSPasteboard.pasteboardWithName({name:?}).releaseGlobally"
        ));
    }
}
