//! Terminal adapter facade. Streaming, panels, and input prompts stay separate.
mod help;
pub mod panel;
mod renderer;
pub mod stream;
pub mod theme;
pub mod todo;

pub use crate::presentation::nudge_line;
use builder_tools::Action;
use console::style;
pub use help::help;
pub use renderer::Renderer;
use std::io::{self, IsTerminal, Write};

pub fn safe(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    stream::Sanitizer::default().push(text, &mut out);
    out
}

/// Shorten a path under the user's home so panels and the banner stay narrow.
pub fn short_path(path: &std::path::Path) -> String {
    std::env::var_os("HOME")
        .and_then(|home| path.strip_prefix(home).ok())
        .map_or_else(
            || path.display().to_string(),
            |relative| format!("~/{}", relative.display()),
        )
}

pub fn banner(profile: &str, model: &str, workspace: &std::path::Path, session: &str, mode: &str) {
    let width = (console::Term::stderr().size().1 as usize)
        .saturating_sub(4)
        .clamp(1, 76);
    let fit = |text: &str| crate::input::layout::clip(&safe(text), width);
    eprintln!(
        "\n  {}  {}",
        theme::accent("builder"),
        theme::muted(env!("CARGO_PKG_VERSION"))
    );
    let profile = if profile.is_empty() { model } else { profile };
    let session: String = session.chars().take(8).collect();
    let workspace = short_path(workspace);
    eprintln!("  {}", theme::title(&fit(&workspace)));
    eprintln!("  {}", theme::muted(&fit(&format!("{profile} · {mode}"))));
    eprintln!("  {}\n", theme::muted(&fit(&format!("session {session}"))));
}
pub fn print_todos(list: &builder_core::todo::List) {
    let width = (console::Term::stderr().size().1 as usize).clamp(20, 100);
    eprintln!();
    for line in todo::board(list, width) {
        eprintln!("{line}");
    }
}
pub fn approve(action: &Action) -> bool {
    if !io::stdin().is_terminal() {
        return false;
    }
    eprintln!(
        "\n  {}\n{}",
        style("◇ approval required").yellow().bold(),
        safe(&action.description())
    );
    eprint!("  Allow this action? [y/N] ");
    let _ = io::stderr().flush();
    let mut answer = String::new();
    io::stdin().read_line(&mut answer).is_ok() && matches!(answer.trim(), "y" | "Y" | "yes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    #[test]
    fn safe_strips_terminal_escapes_and_controls_but_keeps_text_layout() {
        assert_eq!(safe("plain text"), "plain text");
        assert_eq!(safe("tab\tand\nnewline"), "tab\tand\nnewline");
        assert_eq!(safe("\x1b[31mred\x1b[0m"), "red");
        assert_eq!(safe("title\x1b]0;evil\x07 kept"), "title kept");
        assert_eq!(safe("bell\x07 null\0 cr\r end"), "bell null cr end");
        assert_eq!(safe("ünïcödé · ok"), "ünïcödé · ok");
    }

    #[test]
    fn short_path_collapses_the_home_directory_only() {
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from)
            && home != Path::new("/")
        {
            assert_eq!(short_path(&home.join("Code/builder")), "~/Code/builder");
            assert_eq!(short_path(&home), "~/");
            let elsewhere = Path::new("/nonexistent-root-9f3c/x");
            assert_eq!(short_path(elsewhere), elsewhere.display().to_string());
        }
    }

    #[test]
    fn banner_and_todo_board_render_without_a_terminal() {
        // Both write to stderr only; this checks they tolerate any width and
        // an empty profile name without panicking.
        banner(
            "",
            "qwen3:8b",
            Path::new("/tmp/project"),
            "0123456789abcdef",
            "ask",
        );
        banner(
            "local",
            "qwen3:8b",
            Path::new("/tmp/project"),
            "short",
            "auto",
        );
        let list = builder_core::todo::List {
            items: vec![builder_core::todo::Item {
                content: "write the tests".into(),
                status: builder_core::todo::Status::InProgress,
            }],
        };
        print_todos(&list);
        print_todos(&builder_core::todo::List::default());
    }

    #[test]
    fn approval_is_denied_without_a_terminal() {
        if !io::stdin().is_terminal() {
            let action = Action::from_call(&builder_core::protocol::ToolCall {
                id: "1".into(),
                kind: "function".into(),
                function: builder_core::protocol::Function {
                    name: "shell".into(),
                    arguments: r#"{"command":"rm -rf build"}"#.into(),
                },
            })
            .unwrap();
            assert!(!approve(&action));
        }
    }
}
