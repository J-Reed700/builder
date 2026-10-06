use anyhow::Result;
use builder::ui;
use builder_core::store::Store;

pub(crate) fn list(store: &Store) -> Result<()> {
    let sessions = store.sessions()?;
    if sessions.is_empty() {
        println!("No sessions yet. Run builder to start one.");
    }
    for session in sessions {
        println!(
            "{}  {:<12}  {}  {}",
            &session.id[..8],
            ui::safe(&session.profile),
            &session.updated_at[..19],
            ui::safe(&session.title)
        );
    }
    Ok(())
}

pub(crate) fn export(store: &Store, id: &str, json: bool) -> Result<()> {
    let session = store.resolve(id)?;
    let messages = store.history_messages(&session.id)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(
                &serde_json::json!({"session":session,"messages":messages})
            )?
        );
        return Ok(());
    }

    println!("# {}\n", ui::safe(&session.title));
    for message in messages {
        println!(
            "## {}\n\n{}\n",
            message.role,
            ui::safe(message.content.as_deref().unwrap_or(""))
        );
        for call in message.tool_calls {
            println!(
                "Tool: {}\n\n```json\n{}\n```\n",
                ui::safe(&call.function.name),
                ui::safe(&call.function.arguments)
            );
        }
    }
    Ok(())
}
