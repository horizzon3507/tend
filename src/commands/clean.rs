use std::fs;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::{Value, json};

use crate::cli::CleanArgs;
use crate::commands;
use crate::config;
use crate::scan::{ScanOptions, build_report};
use crate::targets::{self, Target};
use crate::ui;
use crate::util::{human_bytes, tildify};

/// Double guard: the name must still be a known target AND the path must sit inside
/// one of the scanned roots. Nothing gets deleted on the strength of a stale report.
fn is_eligible(path: &Path, target: &Target, roots: &[PathBuf]) -> bool {
    let Some(name) = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
    else {
        return false;
    };
    let Some(expected) = targets::find(&name) else {
        return false;
    };
    if expected.name != target.name {
        return false;
    }
    roots.iter().any(|root| {
        path.strip_prefix(root)
            .map(|rest| !rest.as_os_str().is_empty())
            .unwrap_or(false)
    })
}

fn confirm(question: &str) -> bool {
    if !std::io::stdin().is_terminal() {
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

pub fn run(args: CleanArgs) -> Result<i32> {
    let config = config::load().config;
    let Some(roots) = commands::usable_roots(config::resolved_roots(&config, &args.common.roots))
    else {
        return Ok(1);
    };

    let depth = args.common.depth.unwrap_or(config.max_depth);
    let limit = args.limit.unwrap_or(400);
    let include_unsafe = args.all;

    let report = build_report(&ScanOptions {
        roots: roots.clone(),
        depth,
        stale_days: config.stale_days,
        safe_only: !include_unsafe,
        limit,
        repos: false,
        links: false,
        measure_dirs: false,
    });

    let eligible: Vec<_> = report
        .findings
        .iter()
        .filter(|finding| is_eligible(&finding.path, finding.target, &roots))
        .collect();
    let total: u64 = eligible.iter().map(|finding| finding.bytes).sum();

    if args.common.json {
        let document = json!({
            "roots": roots.iter().map(|root| root.display().to_string()).collect::<Vec<_>>(),
            "apply": args.apply,
            "truncated": report.truncated,
            "total": total,
            "items": eligible.iter().map(|finding| json!({
                "path": finding.path.display().to_string(),
                "bytes": finding.bytes,
                "safe": finding.target.safe,
            })).collect::<Vec<_>>(),
            "protectedPaths": report.protected_paths.iter().map(|path| path.display().to_string()).collect::<Vec<_>>(),
        });

        // One document per run: with --apply the outcome is merged into the plan
        // instead of printing two JSON objects in a row.
        let emit = |removed: Option<(usize, u64, &[String])>| -> Result<i32> {
            let mut fields = document.as_object().cloned().unwrap_or_default();
            if let Some((count, freed, errors)) = removed {
                fields.insert("removed".to_string(), json!(count));
                fields.insert("freed".to_string(), json!(freed));
                fields.insert("errors".to_string(), json!(errors));
            }
            println!("{}", serde_json::to_string_pretty(&Value::Object(fields))?);
            Ok(0)
        };

        if !args.apply {
            emit(None)?;
            return Ok(0);
        }

        if eligible.is_empty() {
            return emit(Some((0, 0, &[])));
        }

        if !args.yes
            && !confirm(&format!(
                "  delete {} directories ({})? [y/N] ",
                eligible.len(),
                human_bytes(total)
            ))
        {
            emit(None)?;
            eprintln!("{}", ui::bad("✗ cancelled, nothing was removed"));
            return Ok(1);
        }

        let (removed, freed, errors) = delete_all(&eligible, true);
        let code = if errors.is_empty() { 0 } else { 1 };
        emit(Some((removed, freed, &errors)))?;
        return Ok(code);
    } else {
        ui::section("tend", Some("cleanup"));
        if args.apply {
            ui::say_warn(&format!(
                "destructive mode, {} will be removed",
                ui::bold(&human_bytes(total))
            ));
        } else {
            ui::say_info("dry run, nothing will be deleted");
        }
        ui::kv(
            "targets",
            &format!(
                "{} items{}",
                eligible.len(),
                if include_unsafe {
                    " (including unsafe)"
                } else {
                    " safe"
                }
            ),
        );
        ui::kv("space", &ui::bold(&human_bytes(total)));
        ui::blank();

        let shown: Vec<_> = eligible.iter().take(24).collect();
        let max = shown.first().map(|finding| finding.bytes).unwrap_or(1);
        for finding in &shown {
            let relative = if max > 0 {
                finding.bytes as f64 / max as f64
            } else {
                0.0
            };
            let tag = if finding.target.safe {
                ui::grey("safe")
            } else {
                ui::warn(&format!("caution · {}", finding.target.note))
            };
            println!(
                "  {}  {}  {} {} {}",
                ui::bold(&ui::pad_visible(&human_bytes(finding.bytes), 9, true)),
                ui::meter(relative, 10),
                tildify(&finding.path),
                ui::dim("·"),
                tag
            );
        }
        if eligible.len() > shown.len() {
            ui::say_note(&format!(
                "  ... {} more items",
                eligible.len() - shown.len()
            ));
        }
        if !report.protected_paths.is_empty() {
            ui::say_note(&format!(
                "  {} tool installs left out (not project caches)",
                report.protected_paths.len()
            ));
        }
        if report.truncated {
            ui::say_warn("scan capped, raise with --limit");
        }
        ui::blank();
    }

    if !args.apply {
        ui::say_info(&format!(
            "to really delete: {}",
            ui::bold("tend clean --apply")
        ));
        return Ok(0);
    }

    if eligible.is_empty() {
        ui::say_note("nothing eligible to remove");
        return Ok(0);
    }

    if !args.yes
        && !confirm(&format!(
            "  delete {} directories ({})? [y/N] ",
            eligible.len(),
            human_bytes(total)
        ))
    {
        ui::say_fail("cancelled, nothing was removed");
        return Ok(1);
    }

    let (removed, freed, errors) = delete_all(&eligible, false);
    ui::blank();
    ui::say_ok(&format!(
        "{removed} directories removed, {} freed",
        ui::bold(&human_bytes(freed))
    ));
    for error in errors.iter().take(5) {
        ui::say_fail(error);
    }
    Ok(if errors.is_empty() { 0 } else { 1 })
}

/// Removes each planned directory. `quiet` suppresses the per-item line, which the
/// JSON mode does not need.
fn delete_all(eligible: &[&crate::scan::Finding], quiet: bool) -> (usize, u64, Vec<String>) {
    let mut freed = 0u64;
    let mut removed = 0usize;
    let mut errors: Vec<String> = Vec::new();

    for finding in eligible {
        match fs::remove_dir_all(&finding.path) {
            Ok(()) => {
                freed += finding.bytes;
                removed += 1;
                if !quiet {
                    ui::bullet(
                        &format!(
                            "{} {}",
                            ui::grey(&tildify(&finding.path)),
                            ui::dim(&human_bytes(finding.bytes))
                        ),
                        2,
                    );
                }
            }
            Err(error) => errors.push(format!("{}: {error}", finding.path.display())),
        }
    }

    (removed, freed, errors)
}
