use anyhow::Result;

use crate::cli::HistoryArgs;
use crate::history;
use crate::ui;
use crate::util::{human_age, human_bytes, now_ms};

pub fn run(args: HistoryArgs) -> Result<i32> {
    let entries = history::read(args.limit);

    if args.json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(0);
    }

    ui::section("tend", Some("history"));
    if entries.is_empty() {
        ui::say_note("no cleanup history yet");
        return Ok(0);
    }

    let now = now_ms();
    let rows: Vec<Vec<String>> = entries
        .iter()
        .map(|entry| {
            vec![
                human_age(now - entry.at),
                entry.command.clone(),
                human_bytes(entry.freed),
                entry.removed.to_string(),
                entry.errors.to_string(),
            ]
        })
        .collect();
    ui::table(&["when", "command", "freed", "removed", "errors"], &rows);
    Ok(0)
}
