//! The agent's ordered working checklist. A list is never separate mutable
//! state: it is the latest successful `todo_write` request in active original
//! history, so rewind, compaction and restart agree with the journal.
use crate::{
    protocol::Message,
    store::{Store, ToolOutcome},
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub const TOOL: &str = "todo_write";
pub const MAX_ITEMS: usize = 20;
pub const MAX_ITEM_BYTES: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Item {
    pub content: String,
    pub status: Status,
}

/// A complete replacement list. Writes are whole-list, so a lost or repeated
/// update can never leave items half-applied.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct List {
    pub items: Vec<Item>,
}

impl List {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.items.len() <= MAX_ITEMS,
            "A todo list holds at most {MAX_ITEMS} items; merge or drop steps"
        );
        for (index, item) in self.items.iter().enumerate() {
            let number = index + 1;
            ensure!(
                !item.content.trim().is_empty(),
                "Todo item {number} is empty"
            );
            ensure!(
                item.content.len() <= MAX_ITEM_BYTES,
                "Todo item {number} exceeds {MAX_ITEM_BYTES} bytes; state one concrete step"
            );
            ensure!(
                !item.content.contains(['\n', '\r']),
                "Todo item {number} must be a single line"
            );
        }
        ensure!(
            self.items
                .iter()
                .filter(|item| item.status == Status::InProgress)
                .count()
                <= 1,
            "At most one todo item may be in_progress"
        );
        Ok(())
    }

    pub fn completed(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.status == Status::Completed)
            .count()
    }

    pub fn is_finished(&self) -> bool {
        self.completed() == self.items.len()
    }

    /// The item to work on: the one in progress, else the first pending one.
    pub fn current(&self) -> Option<(usize, &Item)> {
        self.items
            .iter()
            .enumerate()
            .find(|(_, item)| item.status == Status::InProgress)
            .or_else(|| {
                self.items
                    .iter()
                    .enumerate()
                    .find(|(_, item)| item.status == Status::Pending)
            })
            .map(|(index, item)| (index + 1, item))
    }

    /// Plain numbered checklist for model-facing text.
    pub fn checklist(&self) -> String {
        self.items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let mark = match item.status {
                    Status::Completed => "[x]",
                    Status::InProgress => "[>]",
                    Status::Pending => "[ ]",
                };
                format!("{mark} {}. {}", index + 1, item.content.trim())
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The tool result recorded in history after a successful write.
    pub fn receipt(&self) -> String {
        if self.items.is_empty() {
            return "Todo list cleared.".into();
        }
        let total = self.items.len();
        let head = match self.current() {
            Some((number, item)) => format!(
                "Todo list saved: {} of {total} completed. Current item: {number}. {}\nWork on the current item now. When it is done, call {TOOL} with it completed and the next item in_progress in the same response as your next action.",
                self.completed(),
                item.content.trim()
            ),
            None => format!(
                "Todo list saved: all {total} items completed. Verify the result, then report it to the user."
            ),
        };
        format!("{head}\n{}", self.checklist())
    }

    fn decode(arguments: &str) -> Option<Self> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Arguments {
            todos: List,
        }
        let list = serde_json::from_str::<Arguments>(arguments).ok()?.todos;
        list.validate().ok().map(|()| list)
    }

    /// The newest successfully recorded list. `history` must be active
    /// original rows, not a compacted projection, so a checkpoint cannot hide
    /// the plan and a rewound turn cannot keep it.
    pub fn latest(history: &[Message], outcomes: &HashMap<String, ToolOutcome>) -> Option<Self> {
        history
            .iter()
            .rev()
            .flat_map(|message| message.tool_calls.iter().rev())
            .filter(|call| {
                call.function.name == TOOL
                    && outcomes.get(&call.id) == Some(&ToolOutcome::Succeeded)
            })
            .find_map(|call| Self::decode(&call.function.arguments))
    }
}

impl Store {
    pub fn todos(&self, session: &str) -> Result<Option<List>> {
        Ok(List::latest(
            &self.history_messages(session)?,
            &self.tool_outcomes(session)?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Function, Role, ToolCall};
    use serde_json::json;

    fn item(content: &str, status: Status) -> Item {
        Item {
            content: content.into(),
            status,
        }
    }

    fn write(id: &str, todos: serde_json::Value) -> Message {
        let mut message = Message::text(Role::Assistant, "");
        message.tool_calls.push(ToolCall {
            id: id.into(),
            kind: "function".into(),
            function: Function {
                name: TOOL.into(),
                arguments: json!({ "todos": todos }).to_string(),
            },
        });
        message
    }

    #[test]
    fn validation_bounds_items_and_allows_one_active_step() {
        let ok = List {
            items: vec![
                item("read the parser", Status::Completed),
                item("fix the parser", Status::InProgress),
                item("run the tests", Status::Pending),
            ],
        };
        ok.validate().unwrap();
        assert_eq!(ok.current().unwrap().0, 2);
        assert_eq!(ok.completed(), 1);
        assert!(!ok.is_finished());

        let mut two_active = ok.clone();
        two_active.items[2].status = Status::InProgress;
        assert!(two_active.validate().is_err());
        for bad in ["", "  ", "two\nlines", &"x".repeat(MAX_ITEM_BYTES + 1)] {
            assert!(
                List {
                    items: vec![item(bad, Status::Pending)]
                }
                .validate()
                .is_err()
            );
        }
        assert!(
            List {
                items: vec![item("step", Status::Pending); MAX_ITEMS + 1]
            }
            .validate()
            .is_err()
        );
        List::default().validate().unwrap();
        assert!(List::default().is_finished());
    }

    #[test]
    fn current_falls_back_to_the_first_pending_item() {
        let list = List {
            items: vec![
                item("done", Status::Completed),
                item("next", Status::Pending),
                item("later", Status::Pending),
            ],
        };
        assert_eq!(
            list.current().map(|(n, i)| (n, i.content.as_str())),
            Some((2, "next"))
        );
        assert!(list.receipt().contains("Current item: 2. next"));
        assert!(list.checklist().starts_with("[x] 1. done\n[ ] 2. next"));
    }

    #[test]
    fn latest_uses_only_successful_valid_writes() {
        let first = json!([{"content":"inspect","status":"in_progress"}]);
        let second = json!([{"content":"inspect","status":"completed"},{"content":"edit","status":"in_progress"}]);
        let invalid =
            json!([{"content":"a","status":"in_progress"},{"content":"b","status":"in_progress"}]);
        let history = vec![
            Message::text(Role::User, "task"),
            write("first", first),
            Message::tool("first", "saved".into()),
            write("second", second),
            Message::tool("second", "saved".into()),
            write("failed", json!([])),
            Message::tool("failed", "ERROR".into()),
            write("invalid", invalid),
            Message::tool("invalid", "saved".into()),
        ];
        let mut outcomes = HashMap::from([
            ("first".to_string(), ToolOutcome::Succeeded),
            ("second".to_string(), ToolOutcome::Succeeded),
            ("failed".to_string(), ToolOutcome::Failed),
            ("invalid".to_string(), ToolOutcome::Succeeded),
        ]);
        let list = List::latest(&history, &outcomes).unwrap();
        assert_eq!(list.completed(), 1);
        assert_eq!(list.current().unwrap().1.content, "edit");

        // A later successful empty write clears the board.
        outcomes.insert("failed".into(), ToolOutcome::Succeeded);
        assert!(List::latest(&history, &outcomes).unwrap().items.is_empty());

        // Without any successful write there is no list.
        assert!(List::latest(&history[..3], &HashMap::new()).is_none());
    }

    #[test]
    fn a_list_survives_later_user_turns_until_replaced() {
        let history = vec![
            Message::text(Role::User, "task"),
            write("plan", json!([{"content":"edit","status":"pending"}])),
            Message::tool("plan", "saved".into()),
            Message::text(Role::User, "why so slow? keep going"),
        ];
        let outcomes = HashMap::from([("plan".to_string(), ToolOutcome::Succeeded)]);
        assert_eq!(List::latest(&history, &outcomes).unwrap().items.len(), 1);
    }
}
