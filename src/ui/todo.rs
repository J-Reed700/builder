//! The todo board: the agent's ordered plan, redrawn whenever it changes.
use super::{safe, theme};
use builder_core::todo::{List, Status};
use console::style;

const INDENT: &str = "    ";
/// Marker column plus a space, after `INDENT`.
const ITEM_INDENT: &str = "      ";

/// Styled board lines, each already indented, wrapped to `width` columns.
pub fn board(list: &List, width: usize) -> Vec<String> {
    let total = list.items.len();
    let progress = match total {
        0 => "cleared".to_owned(),
        _ if list.is_finished() => format!("all {total} done"),
        _ => format!("{} of {total} done", list.completed()),
    };
    let mut lines = vec![format!(
        "{INDENT}{}  {}",
        theme::accent("Todo"),
        theme::muted(&progress)
    )];
    let text_width = width.saturating_sub(ITEM_INDENT.len() + 2).max(12);
    for item in &list.items {
        let (marker, paint): (String, fn(&str) -> String) = match item.status {
            Status::Completed => (theme::success("✓"), done),
            Status::InProgress => (theme::accent("▸"), active),
            Status::Pending => (theme::muted("○"), plain),
        };
        let wrapped = crate::input::layout::wrap(&safe(item.content.trim()), text_width);
        for (index, line) in wrapped.iter().enumerate() {
            let lead = if index == 0 { &marker } else { " " };
            lines.push(format!("{ITEM_INDENT}{lead} {}", paint(line)));
        }
    }
    lines
}

fn done(text: &str) -> String {
    style(text).dim().strikethrough().to_string()
}
fn active(text: &str) -> String {
    style(text).bold().to_string()
}
fn plain(text: &str) -> String {
    text.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use builder_core::todo::Item;

    fn plain_lines(list: &List, width: usize) -> Vec<String> {
        board(list, width)
            .iter()
            .map(|line| console::strip_ansi_codes(line).into_owned())
            .collect()
    }

    #[test]
    fn board_marks_each_status_and_counts_progress() {
        let list = List {
            items: vec![
                Item {
                    content: "Read the composer".into(),
                    status: Status::Completed,
                },
                Item {
                    content: "Add the /compact command".into(),
                    status: Status::InProgress,
                },
                Item {
                    content: "Render the divider".into(),
                    status: Status::Pending,
                },
            ],
        };
        assert_eq!(
            plain_lines(&list, 80),
            [
                "    Todo  1 of 3 done",
                "      ✓ Read the composer",
                "      ▸ Add the /compact command",
                "      ○ Render the divider",
            ]
        );
    }

    #[test]
    fn long_items_wrap_under_their_text_and_control_bytes_are_removed() {
        let list = List {
            items: vec![Item {
                content: "wire the repository port \x1b[31mto the summaries table and the service"
                    .into(),
                status: Status::Pending,
            }],
        };
        let lines = plain_lines(&list, 34);
        assert!(lines.len() > 2, "{lines:?}");
        assert!(lines[2].starts_with("        "), "{lines:?}");
        assert!(lines.iter().all(|line| !line.contains("[31m")), "{lines:?}");
    }

    #[test]
    fn empty_and_finished_lists_say_so() {
        assert_eq!(plain_lines(&List::default(), 80), ["    Todo  cleared"]);
        let finished = List {
            items: vec![Item {
                content: "ship".into(),
                status: Status::Completed,
            }],
        };
        assert_eq!(plain_lines(&finished, 80)[0], "    Todo  all 1 done");
    }
}
