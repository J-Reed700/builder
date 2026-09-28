//! Service definitions are generated for review, never installed implicitly.
use anyhow::{Result, ensure};
use clap::ValueEnum;
use std::path::Path;

#[derive(Clone, Copy, ValueEnum)]
pub enum ServiceFormat {
    Systemd,
    Launchd,
}

pub fn render(
    format: ServiceFormat,
    executable: &Path,
    explicit_home: Option<&Path>,
    data_home: &Path,
) -> Result<String> {
    let mut arguments = vec![
        executable
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("Executable path must be UTF-8"))?
            .to_owned(),
    ];
    if let Some(home) = explicit_home {
        arguments.extend([
            "--home".into(),
            home.canonicalize()?
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("Home path must be UTF-8"))?
                .to_owned(),
        ]);
    }
    arguments.push("daemon".into());
    ensure!(
        arguments.iter().all(|a| !a.chars().any(char::is_control)),
        "Service arguments cannot contain control characters"
    );
    match format {
        ServiceFormat::Systemd => {
            let args = arguments
                .iter()
                .map(|arg| {
                    format!(
                        "\"{}\"",
                        arg.replace('\\', "\\\\")
                            .replace('"', "\\\"")
                            .replace('%', "%%")
                            .replace('$', "$$")
                    )
                })
                .collect::<Vec<_>>()
                .join(" ");
            Ok(format!(
                "[Unit]\nDescription=Builder local automation runner\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nType=simple\nExecStart={args}\nRestart=on-failure\nRestartSec=10\nTimeoutStopSec=30\nUMask=0077\n\n[Install]\nWantedBy=default.target\n"
            ))
        }
        ServiceFormat::Launchd => {
            fn xml(text: &str) -> String {
                text.replace('&', "&amp;")
                    .replace('<', "&lt;")
                    .replace('>', "&gt;")
                    .replace('"', "&quot;")
                    .replace('\'', "&apos;")
            }
            let args = arguments
                .iter()
                .map(|a| format!("    <string>{}</string>", xml(a)))
                .collect::<Vec<_>>()
                .join("\n");
            let log = xml(&data_home.join("scheduler.log").display().to_string());
            Ok(format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n  <key>Label</key><string>ai.builder.scheduler</string>\n  <key>ProgramArguments</key>\n  <array>\n{args}\n  </array>\n  <key>RunAtLoad</key><true/>\n  <key>KeepAlive</key><true/>\n  <key>ThrottleInterval</key><integer>10</integer>\n  <key>Umask</key><integer>63</integer>\n  <key>StandardErrorPath</key><string>{log}</string>\n</dict>\n</plist>\n"
            ))
        }
    }
}
