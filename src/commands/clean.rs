use std::fs;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::Result;
use serde_json::{Value, json};

use crate::cli::CleanArgs;
use crate::commands;
use crate::config;
use crate::history;
use crate::scan::{ScanOptions, build_report};
use crate::targets::{self, Target};
use crate::ui;
use crate::util::{human_bytes, now_ms, parse_size, tildify};

/// The human plan prints at most this many rows; the deletion plan itself is complete.
const DISPLAY_CAP: usize = 24;

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

/// Active `--older-than` / `--min-size` filters. The cutoff is computed once so a
/// long walk cannot drift while it runs.
struct Filters {
    older_than_days: Option<i64>,
    min_size: Option<u64>,
    cutoff_ms: Option<i64>,
}

impl Filters {
    fn new(older_than: Option<i64>, min_size: Option<String>) -> Result<Self> {
        if let Some(days) = older_than {
            if days < 0 {
                anyhow::bail!("--older-than must be 0 or more days");
            }
        }
        let min_size = match min_size.as_deref() {
            Some(raw) => Some(parse_size(raw).ok_or_else(|| {
                anyhow::anyhow!("invalid --min-size: {raw} (expected e.g. 100MB)")
            })?),
            None => None,
        };
        Ok(Filters {
            older_than_days: older_than,
            min_size,
            cutoff_ms: older_than.map(|days| now_ms() - days.saturating_mul(86_400_000)),
        })
    }

    /// Applies both filters to one finding. A directory with an unreadable mtime is
    /// skipped: the safer outcome when the user asked for "old enough".
    fn retains(&self, path: &Path, bytes: u64) -> bool {
        if let Some(min) = self.min_size {
            if bytes < min {
                return false;
            }
        }
        if let Some(cutoff) = self.cutoff_ms {
            let Some(modified) = modified_ms(path) else {
                return false;
            };
            if modified > cutoff {
                return false;
            }
        }
        true
    }

    fn labels(&self) -> Vec<String> {
        let mut labels = Vec::new();
        if let Some(days) = self.older_than_days {
            labels.push(format!("older than {days} days"));
        }
        if let Some(min) = self.min_size {
            labels.push(format!("at least {}", human_bytes(min)));
        }
        labels
    }
}

fn modified_ms(path: &Path) -> Option<i64> {
    let modified = fs::metadata(path).and_then(|meta| meta.modified()).ok()?;
    modified
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|since| since.as_millis() as i64)
}

fn confirm(question: &str) -> bool {
    confirm_with(question, std::io::stdin().is_terminal())
}

/// Split out so the non-interactive refusal is testable without touching real stdin.
fn confirm_with(question: &str, interactive: bool) -> bool {
    if !interactive {
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

    let filters = Filters::new(args.older_than, args.min_size.clone())?;

    let depth = args.common.depth.unwrap_or(config.max_depth);
    let include_unsafe = args.all;

    let report = build_report(&ScanOptions {
        roots: roots.clone(),
        exclude: config
            .exclude
            .iter()
            .map(|entry| crate::util::expand_home(entry))
            .collect(),
        depth,
        stale_days: config.stale_days,
        safe_only: !include_unsafe,
        limit: usize::MAX,
        repos: false,
        links: false,
        measure_dirs: false,
        progress: false,
    });

    let eligible: Vec<_> = report
        .findings
        .iter()
        .filter(|finding| is_eligible(&finding.path, finding.target, &roots))
        .filter(|finding| filters.retains(&finding.path, finding.bytes))
        .collect();
    let total: u64 = eligible.iter().map(|finding| finding.bytes).sum();
    let filter_labels = filters.labels();

    if args.common.json {
        let document = json!({
            "roots": roots.iter().map(|root| root.display().to_string()).collect::<Vec<_>>(),
            "apply": args.apply,
            "truncated": report.truncated,
            "olderThan": args.older_than,
            "minSize": args.min_size,
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
        history::record("clean", &roots, removed, freed, errors.len());
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
        if !filter_labels.is_empty() {
            ui::kv("filters", &filter_labels.join(" · "));
        }
        ui::blank();

        let shown: Vec<_> = eligible.iter().take(DISPLAY_CAP).collect();
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
            ui::say_warn(&format!(
                "showing the {DISPLAY_CAP} largest of {} items; the full plan is used for deletion",
                eligible.len()
            ));
        }
        if !report.protected_paths.is_empty() {
            ui::say_note(&format!(
                "  {} tool installs left out (not project caches)",
                report.protected_paths.len()
            ));
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
    history::record("clean", &roots, removed, freed, errors.len());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_interactive_confirmation_refuses() {
        assert!(!confirm_with("delete? ", false));
    }

    #[test]
    fn rejects_negative_age_and_bad_sizes() {
        assert!(Filters::new(Some(-1), None).is_err());
        assert!(Filters::new(None, Some("ten megabytes".to_string())).is_err());
        assert!(Filters::new(Some(0), Some("512k".to_string())).is_ok());
    }

    #[test]
    fn min_size_skips_smaller_findings() {
        let filters = Filters::new(None, Some("1MB".to_string())).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        assert!(!filters.retains(tmp.path(), 512));
        assert!(filters.retains(tmp.path(), 2 * 1024 * 1024));
    }

    #[test]
    fn older_than_skips_fresh_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let fresh = tmp.path().join("fresh");
        fs::create_dir(&fresh).unwrap();

        assert!(!Filters::new(Some(7), None).unwrap().retains(&fresh, 4096));
        assert!(Filters::new(Some(0), None).unwrap().retains(&fresh, 4096));
    }

    #[test]
    fn unreadable_mtime_is_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let filters = Filters::new(Some(1), None).unwrap();
        assert!(!filters.retains(&tmp.path().join("gone"), 4096));
    }
}
