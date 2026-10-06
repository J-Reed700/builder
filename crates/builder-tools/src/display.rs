use crate::SUBAGENT_TOOL;
use serde_json::Value;

/// One bounded, single-line description of a requested call, for status display.
/// Built from raw arguments so a malformed request still shows what was asked.
/// The text is untrusted data; callers sanitize and clip it for their terminal.
pub fn call_summary(name: &str, arguments: &str) -> String {
    let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
    let field = |key: &str| args[key].as_str().unwrap_or_default().trim();
    let summary = match name {
        "read_file" => match (
            field("path"),
            args["start_line"].as_u64(),
            args["end_line"].as_u64(),
        ) {
            (path, Some(start), Some(end)) => format!("{path}:{start}-{end}"),
            (path, _, _) => path.to_owned(),
        },
        "list_files" => match field("glob") {
            "" => "**/*".to_owned(),
            glob => glob.to_owned(),
        },
        "search" => match (field("query"), field("glob")) {
            (query, "") => query.to_owned(),
            (query, glob) => format!("{query} in {glob}"),
        },
        "code_search" => field("query").to_owned(),
        SUBAGENT_TOOL => field("description").to_owned(),
        "write_file" | "edit_file" => field("path").to_owned(),
        "multi_edit" => match args["edits"].as_array().map(Vec::len) {
            Some(count) => format!("{} · {count} edits", field("path")),
            None => field("path").to_owned(),
        },
        "shell" => field("command").to_owned(),
        "memory_search" => field("query").to_owned(),
        "memory_get" | "memory_upsert" | "memory_forget" => field("key").to_owned(),
        builder_core::todo::TOOL => {
            match serde_json::from_value::<builder_core::todo::List>(args["todos"].clone()) {
                Ok(list) if list.items.is_empty() => "clear".to_owned(),
                Ok(list) => match list.current() {
                    Some((_, item)) => format!(
                        "{}/{} · {}",
                        list.completed(),
                        list.items.len(),
                        item.content.trim()
                    ),
                    None => format!("{0}/{0} done", list.items.len()),
                },
                Err(_) => String::new(),
            }
        }
        "research" => {
            let request = &args["request"];
            let operation = request["operation"].as_str().unwrap_or("?");
            match request["criterion"]
                .as_str()
                .or_else(|| request["query"].as_str())
                .or_else(|| request["command"].as_str())
                .or_else(|| request["claim"].as_str())
                .or_else(|| request["question"].as_str())
                .or_else(|| request["key"].as_str())
                .or_else(|| request["outcome"].as_str())
            {
                Some(detail) => format!("{operation} · {}", detail.trim()),
                None => operation.to_owned(),
            }
        }
        _ => String::new(),
    };
    one_line(&summary, 240)
}

/// A short outcome note for a completed call: the reason a call failed, or the
/// shape of what a successful call returned. Never a claim the tool succeeded.
pub fn result_note(name: &str, result: &str) -> Option<String> {
    for prefix in ["ERROR:", "DENIED:"] {
        if let Some(reason) = result.strip_prefix(prefix) {
            return Some(one_line(reason, 160));
        }
    }
    let note = match name {
        // Shell reports its own exit status; a nonzero exit is a successful
        // execution with a failing command, and the distinction must be visible.
        // A clean exit needs no note; the nonzero case is the one a user must
        // not mistake for success just because the tool call itself worked.
        "shell" => match result
            .strip_prefix("exit: ")
            .and_then(|rest| rest.split('\n').next())
        {
            Some("0" | "exit status: 0" | "exit code: 0") | None => return None,
            Some(status) => format!(
                "exit {}",
                status
                    .strip_prefix("exit status: ")
                    .or_else(|| status.strip_prefix("exit code: "))
                    .unwrap_or(status)
            ),
        },
        SUBAGENT_TOOL => result
            .lines()
            .next()?
            .split(" · ")
            .find(|part| part.ends_with(" tool calls"))?
            .to_owned(),
        "read_file" | "search" | "list_files" | "code_search" => {
            let lines = result.lines().filter(|line| !line.is_empty()).count();
            format!("{lines} lines")
        }
        _ => return None,
    };
    Some(one_line(&note, 160))
}

fn one_line(text: &str, max: usize) -> String {
    let mut out = String::with_capacity(text.len().min(max + 1));
    let mut space = true;
    for c in text.trim().chars() {
        if out.chars().count() >= max {
            out.push('…');
            break;
        }
        if c.is_whitespace() {
            if !space {
                out.push(' ');
                space = true;
            }
        } else {
            out.push(c);
            space = false;
        }
    }
    out.trim_end().to_owned()
}
