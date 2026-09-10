//! Bounded local Git history capture for retrieval priors. Historical entries
//! are never current-source evidence and callers must label them accordingly.
use anyhow::{Context, Result, ensure};
use builder_core::{
    code_index::{CodeHistoryEntry, CodeHistorySnapshot},
    memory::digest,
};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::io::AsyncReadExt;

use crate::Workspace;

const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

pub async fn capture(
    workspace: &Workspace,
    commits: usize,
    timeout_secs: u64,
) -> Result<Option<CodeHistorySnapshot>> {
    ensure!((1..=5000).contains(&commits), "Invalid Git history limit");
    ensure!(
        (1..=30).contains(&timeout_secs),
        "Invalid Git history timeout"
    );
    let Some(head) = git_head(workspace.root(), timeout_secs).await? else {
        return Ok(None);
    };
    let output = git_output(
        workspace.root(),
        &[
            "log",
            "--no-renames",
            "--format=format:%x1e%H%x1f%ct%x1f%s",
            "--name-only",
            "-n",
            &commits.to_string(),
        ],
        timeout_secs,
    )
    .await?;
    let entries = parse(&output)?;
    let snapshot_hash = digest(&serde_json::to_vec(&(&head, &entries))?);
    Ok(Some(CodeHistorySnapshot {
        head,
        snapshot_hash,
        entries,
    }))
}

async fn git_head(root: &Path, timeout_secs: u64) -> Result<Option<String>> {
    let mut command = tokio::process::Command::new("git");
    command
        .current_dir(root)
        .args(["rev-parse", "--verify", "HEAD"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().context("Could not start local Git")?;
    let stdout = child.stdout.take().context("Git stdout unavailable")?;
    let result = tokio::time::timeout(Duration::from_secs(timeout_secs), async {
        let mut bytes = Vec::new();
        stdout
            .take(256)
            .read_to_end(&mut bytes)
            .await
            .context("Could not read Git HEAD")?;
        let status = child.wait().await.context("Could not wait for Git HEAD")?;
        Ok::<_, anyhow::Error>((status.success(), bytes))
    })
    .await
    .context("Git HEAD lookup timed out")??;
    if !result.0 {
        return Ok(None);
    }
    let head = String::from_utf8(result.1)?;
    let head = head.trim();
    ensure!(
        !head.is_empty() && head.len() <= 128,
        "Git returned an invalid HEAD"
    );
    Ok(Some(head.into()))
}

async fn git_output(root: &Path, args: &[&str], timeout_secs: u64) -> Result<String> {
    let mut command = tokio::process::Command::new("git");
    command
        .current_dir(root)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().context("Could not start local Git")?;
    let stdout = child.stdout.take().context("Git stdout unavailable")?;
    let bytes = tokio::time::timeout(Duration::from_secs(timeout_secs), async {
        let mut bytes = Vec::new();
        stdout
            .take(MAX_OUTPUT_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .await
            .context("Could not read local Git history")?;
        let status = child
            .wait()
            .await
            .context("Could not wait for local Git history")?;
        ensure!(status.success(), "Local Git history command failed");
        Ok::<_, anyhow::Error>(bytes)
    })
    .await
    .context("Local Git history capture timed out")??;
    ensure!(
        bytes.len() <= MAX_OUTPUT_BYTES,
        "Local Git history exceeds output bound"
    );
    String::from_utf8(bytes).context("Local Git history is not UTF-8")
}

fn parse(output: &str) -> Result<Vec<CodeHistoryEntry>> {
    let mut entries = Vec::new();
    for record in output
        .split('\u{1e}')
        .filter(|record| !record.trim().is_empty())
    {
        let mut lines = record.trim_matches(['\r', '\n']).lines();
        let header = lines.next().context("Git history record has no header")?;
        let mut fields = header.splitn(3, '\u{1f}');
        let revision = fields.next().unwrap_or_default().trim();
        let unix_time = fields
            .next()
            .context("Git history record has no timestamp")?
            .parse::<i64>()?;
        let subject = sanitize(fields.next().unwrap_or_default(), 1000);
        ensure!(
            !revision.is_empty() && revision.len() <= 128,
            "Git history record has invalid revision"
        );
        let mut paths = lines
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .map(|path| sanitize(path, 4096))
            .collect::<Vec<_>>();
        paths.sort();
        paths.dedup();
        if paths.len() > 512 {
            paths.truncate(512);
        }
        entries.push(CodeHistoryEntry {
            revision: revision.into(),
            unix_time,
            subject,
            paths,
        });
    }
    Ok(entries)
}

fn sanitize(value: &str, max: usize) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .take(max)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bounded_commit_records_and_changed_paths() {
        let entries = parse(
            "\u{1e}abc\u{1f}100\u{1f}Reduce arena shield drops\n\nsrc/arena.rs\n\
             \u{1e}def\u{1f}90\u{1f}Boss collision\n\nsrc/collision.rs\n",
        )
        .unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].subject, "Reduce arena shield drops");
        assert_eq!(entries[0].paths, ["src/arena.rs"]);
        assert_eq!(entries[1].revision, "def");
    }
}
