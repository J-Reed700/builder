//! Plain activity descriptions shared by terminal and remote adapters.
//! No terminal, HTTP, or runtime state belongs here.

/// The user-facing record of a progress nudge. Shared with remote clients.
pub fn nudge_line(
    calls: usize,
    step: Option<(usize, usize)>,
    repeated_reads: usize,
    planning: bool,
) -> String {
    let mut line = format!("{calls} actions without new evidence or a file change");
    if repeated_reads > 0 {
        line.push_str(&format!(
            " · {repeated_reads} repeated read{}",
            if repeated_reads == 1 { "" } else { "s" }
        ));
    }
    match step {
        Some((number, total)) => {
            line.push_str(&format!(" · nudged back to todo step {number} of {total}"))
        }
        None if planning => line.push_str(" · no plan yet, nudged to write a todo list"),
        None => line.push_str(" · nudged to act on what it found"),
    }
    line
}
