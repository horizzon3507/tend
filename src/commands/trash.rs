use std::io::{IsTerminal, Write};

use anyhow::Result;
use serde_json::json;

use crate::cli::{TrashAction, TrashArgs};
use crate::trash;
use crate::ui;
use crate::util::{human_age, human_bytes, is_tty, now_ms, tildify};

fn confirm(question: &str) -> bool {
    if !(is_tty() && std::io::stdin().is_terminal()) {
        return false;
    }
    print!("{question}");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim().to_lowercase().as_str(), "y" | "yes")
}

fn list(args: &TrashArgs) -> Result<i32> {
    let entries = trash::list();
    let total = trash::total_bytes();

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "items": entries.iter().map(|entry| json!({
                    "id": entry.id,
                    "originalPath": entry.original_path,
                    "trashedAt": entry.trashed_at,
                    "bytes": entry.bytes,
                    "source": entry.source,
                })).collect::<Vec<_>>(),
                "total": total,
            }))?
        );
        return Ok(0);
    }

    ui::section("tend", Some("trash"));
    if entries.is_empty() {
        ui::say_note("the trash is empty");
        return Ok(0);
    }
    ui::kv(
        "items",
        &format!("{} ({})", entries.len(), ui::bold(&human_bytes(total))),
    );
    ui::blank();
    for entry in &entries {
        let age = now_ms() - entry.trashed_at;
        let age_text = if age < 60_000 {
            "just now".to_string()
        } else {
            format!("{} ago", human_age(age))
        };
        println!(
            "  {}  {}  {}  {}",
            ui::dim(&entry.id),
            ui::bold(&ui::pad_visible(&human_bytes(entry.bytes), 9, true)),
            ui::grey(&format!("{age_text} · {}", entry.source)),
            tildify(std::path::Path::new(&entry.original_path)),
        );
    }
    ui::blank();
    ui::say_info(&format!(
        "restore with {}, free it with {}",
        ui::bold("tend trash restore <id>"),
        ui::bold("tend trash empty")
    ));
    Ok(0)
}

fn restore(reference: Option<String>) -> Result<i32> {
    let reference = reference.unwrap_or_else(|| "latest".to_string());
    match trash::restore(&reference) {
        Ok(path) => {
            ui::say_ok(&format!("restored {}", ui::bold(&tildify(&path))));
            Ok(0)
        }
        Err(error) => {
            ui::say_fail(&error.to_string());
            Ok(1)
        }
    }
}

fn empty(args: &TrashArgs, older_than: Option<i64>, yes: bool) -> Result<i32> {
    let entries = trash::list();
    let cutoff = older_than.map(|days| now_ms() - days.saturating_mul(86_400_000));
    let doomed: Vec<_> = entries
        .iter()
        .filter(|entry| cutoff.map(|cut| entry.trashed_at <= cut).unwrap_or(true))
        .collect();
    let total: u64 = doomed.iter().map(|entry| entry.bytes).sum();

    if doomed.is_empty() {
        if args.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "removed": 0, "freed": 0, "errors": Vec::<String>::new(),
                }))?
            );
        } else {
            ui::say_note("nothing to empty");
        }
        return Ok(0);
    }

    if !yes
        && !confirm(&format!(
            "  permanently delete {} trashed items ({})? [y/N] ",
            doomed.len(),
            human_bytes(total)
        ))
    {
        ui::say_fail("cancelled, the trash was kept");
        return Ok(1);
    }

    let (removed, freed, errors) = trash::empty(older_than);
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "removed": removed,
                "freed": freed,
                "errors": errors,
            }))?
        );
    } else {
        ui::say_ok(&format!(
            "{removed} items deleted, {} freed",
            ui::bold(&human_bytes(freed))
        ));
        for error in errors.iter().take(5) {
            ui::say_fail(error);
        }
    }
    Ok(if errors.is_empty() { 0 } else { 1 })
}

pub fn run(args: TrashArgs) -> Result<i32> {
    match &args.action {
        None | Some(TrashAction::List) => list(&args),
        Some(TrashAction::Restore { reference }) => restore(reference.clone()),
        Some(TrashAction::Empty { older_than, yes }) => empty(&args, *older_than, *yes),
    }
}
