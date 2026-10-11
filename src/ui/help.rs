use super::panel;

/// The `/help` panel: commands grouped by what they do, then the editing keys.
/// Command names are kept in step with the composer's `/` menu by a test.
pub fn help(width: usize) -> String {
    let rows = [
        panel::section("Conversation"),
        panel::field(
            "/attach PATH",
            "Send a workspace file into the conversation",
        ),
        panel::field(
            "/history",
            "Show the active conversation; /history archived shows rewound turns",
        ),
        panel::field("/todo", "Show the agent's current todo list"),
        panel::field("/retry", "Continue an unfinished turn"),
        panel::field("/cancel", "End pending work and keep the completed context"),
        panel::field(
            "/rewind",
            "Archive the last turn and edit its message; workspace files stay changed",
        ),
        panel::field(
            "/clear",
            "Start a fresh conversation and archive the current one",
        ),
        panel::section("Context"),
        panel::field("/status", "Session, context estimate, and recovery state"),
        panel::field(
            "/compact",
            "Summarize context now and preserve the originals",
        ),
        panel::section("Configuration"),
        panel::field(
            "/settings",
            "Pipeline features and budgets for this profile",
        ),
        panel::field(
            "/schedule",
            "Schedule tasks; list, pause, resume, and inspect runs",
        ),
        panel::field("/memory", "Local memory settings and model setup"),
        panel::section("Session"),
        panel::field("/help", "This list"),
        panel::field("/exit", "Save and leave"),
        panel::section("Editing"),
        panel::field("enter", "Send the message"),
        panel::field("alt+enter", "Insert a new line; ctrl+j does the same"),
        panel::field("ctrl+v", "Paste directly from the clipboard (macOS)"),
        panel::field("ctrl+z ctrl+y", "Undo and redo"),
        panel::field(
            "ctrl+w ctrl+u",
            "Delete the previous word, or clear the draft",
        ),
        panel::section("Moving around"),
        panel::field(
            "up down",
            "Move between draft lines; browse history from an empty draft",
        ),
        panel::field("ctrl+p ctrl+n", "Browse history from any draft"),
        panel::field(
            "/",
            "Open the command menu; tab or enter completes, esc closes",
        ),
        panel::field(
            "ctrl+c",
            "Pause a streaming response; a follow-up redirects it",
        ),
        panel::section("Good to know"),
        panel::note(
            "Edits and commands require approval unless --auto or --approval trust is set.",
        ),
        panel::note("Large pastes fold into one block; the full text is sent on enter."),
        panel::note(
            "Every message is saved automatically, and rewound turns stay in /history archived.",
        ),
        panel::note(
            "While context is compacting, enter the next message to queue it for sending when compaction finishes.",
        ),
    ];
    panel::render(
        concat!("builder ", env!("CARGO_PKG_VERSION")),
        "commands and keys",
        &rows,
        width,
    )
}

#[cfg(test)]
mod tests {
    use super::help;
    #[test]
    fn help_lists_every_composer_command_and_nothing_extra() {
        let panel = console::strip_ansi_codes(&help(96)).into_owned();
        let listed: std::collections::BTreeSet<&str> = panel
            .lines()
            .filter_map(|line| line.trim_start().strip_prefix('/'))
            // The `/` row documents the command menu itself, not a command.
            .filter(|rest| rest.starts_with(|c: char| c.is_ascii_alphabetic()))
            .map(|line| line.split_whitespace().next().unwrap_or_default())
            .collect();
        let composer: std::collections::BTreeSet<&str> = crate::input::COMMANDS
            .iter()
            .map(|(command, _)| command.trim().trim_start_matches('/'))
            .collect();
        assert_eq!(listed, composer);
        assert!(panel.contains("builder "), "{panel}");
    }
}
