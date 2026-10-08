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
