use builder_tools::{Action, Workspace};

#[tokio::test]
async fn numbered_reads_preserve_indentation_for_exact_edits() {
    let dir = tempfile::tempdir().unwrap();
    let original = "  const flags = a | b;\n\treturn flags;\n";
    std::fs::write(dir.path().join("test.ts"), original).unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let output = workspace
        .execute(&Action::ReadFile {
            path: "test.ts".into(),
            start_line: Some(1),
            end_line: Some(2),
        })
        .await
        .unwrap();
    let source = output
        .lines()
        .filter_map(|line| {
            let (number, source) = line.split_once('|')?;
            number.trim().parse::<usize>().ok().map(|_| source)
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(source, original.trim_end_matches('\n'));
    workspace
        .execute(&Action::EditFile {
            path: "test.ts".into(),
            old: source.clone(),
            new: source.replace("a | b", "a | c"),
        })
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("test.ts")).unwrap(),
        original.replace("a | b", "a | c")
    );
}

#[tokio::test]
async fn exact_edits_make_backups_and_reject_ambiguous_matches() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("test.rs"), "hello hello").unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let action = Action::EditFile {
        path: "test.rs".into(),
        old: "hello".into(),
        new: "world".into(),
    };
    assert!(workspace.execute(&action).await.is_err());
    let action = Action::EditFile {
        path: "test.rs".into(),
        old: "hello hello".into(),
        new: "world".into(),
    };
    workspace.execute(&action).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("test.rs")).unwrap(),
        "world"
    );
    let backup = std::fs::read_dir(dir.path().join(".builder/backups"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert_eq!(std::fs::read_to_string(backup).unwrap(), "hello hello");
}

#[test]
fn traversal_and_metadata_access_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    assert!(workspace.resolve("../outside.txt").is_err());
    std::fs::create_dir(dir.path().join(".git")).unwrap();
    assert!(workspace.resolve(".git/config").is_err());
}

#[cfg(unix)]
#[test]
fn symlinks_cannot_escape_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    assert!(workspace.resolve("link/file.txt").is_err());
}

#[tokio::test]
async fn search_respects_ignore_files() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
    std::fs::write(dir.path().join("ignored.txt"), "needle").unwrap();
    std::fs::write(dir.path().join("visible.txt"), "needle").unwrap();
    let out = Workspace::new(dir.path())
        .unwrap()
        .execute(&Action::Search {
            query: "needle".into(),
            glob: None,
        })
        .await
        .unwrap();
    assert!(out.contains("visible.txt"));
    assert!(!out.contains("ignored.txt"));
}

#[tokio::test]
async fn large_files_require_a_range_and_ranges_guide_continuation() {
    let dir = tempfile::tempdir().unwrap();
    let content = (1..=1400)
        .map(|n| format!("source line {n}\n"))
        .collect::<String>();
    std::fs::write(dir.path().join("large.ts"), content).unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    // A rangeless read of a large file is still refused with orientation,
    // never a source dump.
    let error = workspace
        .execute(&Action::ReadFile {
            path: "large.ts".into(),
            start_line: None,
            end_line: None,
        })
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("1400 lines"));
    assert!(!error.contains("source line"));
    // A lone start_line reads a forward chunk with an explicit continuation.
    let output = workspace
        .execute(&Action::ReadFile {
            path: "large.ts".into(),
            start_line: Some(480),
            end_line: None,
        })
        .await
        .unwrap();
    assert!(output.contains("  480|source line 480"));
    assert!(output.contains("  979|source line 979"));
    assert!(!output.contains("source line 980"));
    assert!(output.contains("lines 980–1400 remain; continue with start_line=980"));
    // A lone end_line reads the chunk ending there.
    let output = workspace
        .execute(&Action::ReadFile {
            path: "large.ts".into(),
            start_line: None,
            end_line: Some(20),
        })
        .await
        .unwrap();
    assert!(output.contains("    1|source line 1"));
    assert!(output.contains("   20|source line 20"));
    assert!(!output.contains("source line 21"));
    // An over-long range returns a bounded first chunk with an explicit note
    // instead of failing without any source.
    let output = workspace
        .execute(&Action::ReadFile {
            path: "large.ts".into(),
            start_line: Some(1),
            end_line: Some(900),
        })
        .await
        .unwrap();
    assert!(output.contains("  500|source line 500"));
    assert!(!output.contains("  501|source line 501"));
    assert!(output.contains("requested range exceeds 500 lines; showing 1–500"));
    // Out-of-range ends clamp to the file, and a start past the end returns
    // orientation instead of an empty body.
    let output = workspace
        .execute(&Action::ReadFile {
            path: "large.ts".into(),
            start_line: Some(1390),
            end_line: Some(2000),
        })
        .await
        .unwrap();
    assert!(output.contains(" 1400|source line 1400"));
    assert!(!output.contains("remain"));
    let output = workspace
        .execute(&Action::ReadFile {
            path: "large.ts".into(),
            start_line: Some(2000),
            end_line: None,
        })
        .await
        .unwrap();
    assert!(output.contains("beyond the last line 1400"));
    assert!(!output.contains("source line"));
    // An exact range is returned verbatim with its surroundings summarized.
    let output = workspace
        .execute(&Action::ReadFile {
            path: "large.ts".into(),
            start_line: Some(480),
            end_line: Some(510),
        })
        .await
        .unwrap();
    assert!(output.contains("1400 total lines"));
    assert!(output.contains("  480|source line 480"));
    assert!(output.contains("  510|source line 510"));
    assert!(!output.contains("source line 511"));
    assert!(output.contains("lines 1–479 precede this range"));
    assert!(output.contains("lines 511–1400 remain; continue with start_line=511"));
}

#[tokio::test]
async fn long_unicode_lines_are_rejected_and_search_remains_bounded() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("long.ts"), "🦀".repeat(4000)).unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let error = workspace
        .execute(&Action::ReadFile {
            path: "long.ts".into(),
            start_line: Some(1),
            end_line: Some(1),
        })
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("12288 output bytes"));
    assert!(error.contains("No source was returned"));
    assert!(!error.contains('🦀'));
    // A range that overflows the byte budget partway names the exact range
    // to retry instead of leaving the model to guess.
    std::fs::write(
        dir.path().join("wide.ts"),
        (1..=100)
            .map(|n| format!("{n}: {}\n", "x".repeat(300)))
            .collect::<String>(),
    )
    .unwrap();
    let error = workspace
        .execute(&Action::ReadFile {
            path: "wide.ts".into(),
            start_line: Some(1),
            end_line: Some(100),
        })
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("12288 output bytes at line"), "{error}");
    assert!(
        error.contains("Retry with start_line=1 and end_line="),
        "{error}"
    );
    assert!(!error.contains("xxx"), "{error}");
    std::fs::write(
        dir.path().join("matches.ts"),
        format!("needle {}\n", "🦀".repeat(300)).repeat(100),
    )
    .unwrap();
    let output = workspace
        .execute(&Action::Search {
            query: "needle".into(),
            glob: Some("matches.ts".into()),
        })
        .await
        .unwrap();
    assert!(output.len() <= 8192);
    assert!(output.contains("truncated"));
}

#[tokio::test]
async fn small_whole_file_reads_and_fresh_targeted_reads_still_work() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("small.rs");
    std::fs::write(&path, "first\nsecond\n").unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let action = Action::ReadFile {
        path: "small.rs".into(),
        start_line: None,
        end_line: None,
    };
    let output = workspace.execute(&action).await.unwrap();
    assert!(output.contains("2 total lines"));
    assert!(output.contains("    2|second"));
    std::fs::write(path, "changed\n").unwrap();
    let output = workspace.execute(&action).await.unwrap();
    assert!(output.contains("changed"));
    assert!(!output.contains("second"));
}

#[cfg(unix)]
#[tokio::test]
async fn shell_output_is_bounded_and_timeout_is_enforced() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let output = workspace
        .execute(&Action::Shell {
            command: "yes hello | head -c 100000".into(),
            timeout_secs: Some(5),
        })
        .await
        .unwrap();
    assert!(output.len() < 33_000);
    assert!(output.contains("truncated"));
    let error = workspace
        .execute(&Action::Shell {
            command: "sleep 3".into(),
            timeout_secs: Some(1),
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("timed out"));
}

#[tokio::test]
async fn rejected_edit_gives_bounded_exact_anchor_without_changing_file() {
    let dir = tempfile::tempdir().unwrap();
    let source = format!(
        "{}  allowUltimate = false,\n): OrbVariant {{\n{}",
        "// preceding line\n".repeat(290),
        "  more source\n".repeat(40)
    );
    std::fs::write(dir.path().join("large.ts"), &source).unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let error = workspace
        .execute(&Action::EditFile {
            path: "large.ts".into(),
            old: "   allowUltimate = false,\n  ): OrbVariant {".into(),
            new: "incorrect".into(),
        })
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("found 0 matches"));
    assert!(error.contains("anchor is at line 291"));
    assert!(error.contains("\n  allowUltimate = false,\n): OrbVariant {"));
    assert!(error.len() < 2500);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("large.ts")).unwrap(),
        source
    );
    assert!(!dir.path().join(".builder").exists());
    workspace
        .execute(&Action::EditFile {
            path: "large.ts".into(),
            old: "  allowUltimate = false,\n): OrbVariant {".into(),
            new: "  allowUltimate = false,\n  shieldDropRateMul?: number,\n): OrbVariant {".into(),
        })
        .await
        .unwrap();
    assert!(
        std::fs::read_to_string(dir.path().join("large.ts"))
            .unwrap()
            .contains("shieldDropRateMul?: number")
    );
    std::fs::write(
        dir.path().join("long.ts"),
        format!("anchor\n{}", "x".repeat(50000)),
    )
    .unwrap();
    let error = workspace
        .execute(&Action::EditFile {
            path: "long.ts".into(),
            old: "anchor\nmissing".into(),
            new: "unused".into(),
        })
        .await
        .unwrap_err()
        .to_string();
    assert!(error.len() < 2500);
    assert!(error.contains("truncated"));
}

#[tokio::test]
async fn unchanged_writes_and_noop_edits_do_not_claim_progress_or_create_backups() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("same.ts"), "unchanged").unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let result = workspace
        .execute(&Action::WriteFile {
            path: "same.ts".into(),
            content: "unchanged".into(),
        })
        .await
        .unwrap();
    assert!(result.starts_with("UNCHANGED:"));
    assert!(
        workspace
            .execute(&Action::EditFile {
                path: "same.ts".into(),
                old: "unchanged".into(),
                new: "unchanged".into()
            })
            .await
            .unwrap_err()
            .to_string()
            .contains("identical")
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("same.ts")).unwrap(),
        "unchanged"
    );
    assert!(!dir.path().join(".builder").exists());
}

#[test]
fn call_summary_names_the_target_of_every_tool() {
    // A status line that says only "Run command" tells the user nothing about
    // what the agent is doing or whether it is repeating itself.
    let cases = [
        ("shell", r#"{"command":"npm run build"}"#, "npm run build"),
        (
            "read_file",
            r#"{"path":"src/App.tsx","start_line":10,"end_line":40}"#,
            "src/App.tsx:10-40",
        ),
        ("read_file", r#"{"path":"src/App.tsx"}"#, "src/App.tsx"),
        (
            "search",
            r#"{"query":"MakeHomeYours","glob":"**/*.tsx"}"#,
            "MakeHomeYours in **/*.tsx",
        ),
        ("list_files", r#"{}"#, "**/*"),
        (
            "edit_file",
            r#"{"path":"a.rs","old":"x","new":"y"}"#,
            "a.rs",
        ),
        (
            "research",
            r#"{"request":{"operation":"verify","criterion":"tests pass"}}"#,
            "verify · tests pass",
        ),
    ];
    for (name, arguments, expected) in cases {
        assert_eq!(builder_tools::call_summary(name, arguments), expected);
    }
}

#[test]
fn call_summary_is_a_bounded_single_line() {
    // Arguments are untrusted model output: they must not break the display.
    let long = "x".repeat(4000);
    let summary =
        builder_tools::call_summary("shell", &serde_json::json!({"command": long}).to_string());
    assert!(
        summary.chars().count() <= 241,
        "{}",
        summary.chars().count()
    );
    let multiline = builder_tools::call_summary("shell", r#"{"command":"one\n  two\n\n\tthree"}"#);
    assert_eq!(multiline, "one two three");
    // Malformed arguments still identify the tool rather than crashing.
    assert_eq!(builder_tools::call_summary("shell", "{not json"), "");
}

#[test]
fn result_note_reports_failure_and_nonzero_exit() {
    // A shell command that ran fine but failed is a successful tool call with a
    // failing result; the exit status must be visible either way.
    assert_eq!(
        builder_tools::result_note("shell", "exit: 1\nstdout:\n\nstderr:\nboom\n"),
        Some("exit 1".into())
    );
    // A clean exit adds nothing the check mark does not already say.
    assert_eq!(
        builder_tools::result_note("shell", "exit: 0\nstdout:\nok\nstderr:\n"),
        None
    );
    assert_eq!(
        builder_tools::result_note("read_file", "ERROR: No such file: a.rs"),
        Some("No such file: a.rs".into())
    );
    assert_eq!(
        builder_tools::result_note("shell", "DENIED: The user did not authorize this tool."),
        Some("The user did not authorize this tool.".into())
    );
    assert_eq!(
        builder_tools::result_note("read_file", "1 | one\n2 | two\n"),
        Some("2 lines".into())
    );
}
