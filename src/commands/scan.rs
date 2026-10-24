use std::path::Path;

use anyhow::Result;

use crate::cli::ScanArgs;
use crate::commands;
use crate::config;
use crate::scan::{Report, ScanOptions, build_report};
use crate::ui;
use crate::util::{human_age, human_bytes, now_ms, tildify};

pub fn run(args: ScanArgs) -> Result<i32> {
    let config = config::load().config;
    let Some(roots) = commands::usable_roots(config::resolved_roots(&config, &args.common.roots))
    else {
        return Ok(1);
    };

    let depth = args.common.depth.unwrap_or(config.max_depth);
    let stale_days = args.stale_days.unwrap_or(config.stale_days);
    let limit = args.limit.unwrap_or(400);

    let report = build_report(&ScanOptions {
        roots: roots.clone(),
        exclude: config
            .exclude
            .iter()
            .map(|entry| crate::util::expand_home(entry))
            .collect(),
        depth,
        stale_days,
        safe_only: true,
        limit,
        repos: true,
        links: true,
        measure_dirs: true,
        progress: true,
    });

    if args.common.json {
        println!("{}", serde_json::to_string_pretty(&report.to_json())?);
        return Ok(0);
    }

    let label = roots
        .iter()
        .map(|root| tildify(root))
        .collect::<Vec<_>>()
        .join(", ");
    ui::section("tend", Some(&format!("scan of {label}")));
    println!("  {} depth {depth}, up to {limit} items", ui::dim("·"));

    render_disk(&report);
    render_biggest(&report);
    render_targets(&report, args.top.unwrap_or(12));
    render_repos(&report);
    render_links(&report);
    ui::blank();

    Ok(0)
}

fn render_disk(report: &Report) {
    let Some(disk) = &report.disk else {
        return;
    };
    let ratio = relative(disk.used, disk.total);
    let text = format!("{}%", (ratio * 100.0).round());
    ui::kv(
        "disk",
        &format!(
            "{} of {} used ({})",
            ui::severity(ratio, &human_bytes(disk.used)),
            human_bytes(disk.total),
            ui::severity(ratio, &text)
        ),
    );
    ui::kv(
        "free",
        &format!(
            "{} on {}",
            human_bytes(disk.free),
            tildify(Path::new(&disk.mount))
        ),
    );
    ui::kv(
        "",
        &format!("{} {}", ui::bar(ratio, 30), ui::severity(ratio, &text)),
    );
}

fn render_biggest(report: &Report) {
    if report.biggest.is_empty() {
        return;
    }
    ui::section("biggest directories", None);
    let max = report.biggest.first().map(|entry| entry.bytes).unwrap_or(1);
    for entry in &report.biggest {
        println!(
            "  {}  {}  {}",
            ui::bold(&ui::pad_visible(&human_bytes(entry.bytes), 9, true)),
            ui::meter(relative(entry.bytes, max), 14),
            tildify(&entry.path)
        );
    }
}

fn render_targets(report: &Report, top: usize) {
    let reclaim = human_bytes(report.reclaimable_safe);
    ui::section(
        "cleanup targets",
        Some(&format!(
            "{} items, {} reclaimable",
            report.findings.len(),
            ui::bold(&reclaim)
        )),
    );
    if report.findings.is_empty() {
        ui::say_note("  nothing to clean here");
        return;
    }

    let shown: Vec<_> = report.findings.iter().take(top).collect();
    let max = shown.first().map(|finding| finding.bytes).unwrap_or(1);
    for finding in &shown {
        let label = if finding.target.safe {
            ui::grey(finding.target.label)
        } else {
            ui::warn(finding.target.label)
        };
        println!(
            "  {}  {}  {} {} {}",
            ui::bold(&ui::pad_visible(&human_bytes(finding.bytes), 9, true)),
            ui::meter(relative(finding.bytes, max), 12),
            tildify(&finding.path),
            ui::dim("·"),
            label
        );
    }

    if report.findings.len() > shown.len() {
        ui::say_note(&format!(
            "  ... {} more items",
            report.findings.len() - shown.len()
        ));
    }
    if !report.protected_paths.is_empty() {
        ui::say_note(&format!(
            "  {} installs skipped on purpose (global bun, plugins, apps in ~/.local/opt)",
            report.protected_paths.len()
        ));
    }
    if report.truncated {
        ui::say_warn("scan stopped at the item limit, raise with --limit");
    }
    ui::blank();
    ui::say_warn(&format!(
        "run {} to see the removal plan",
        ui::bold("tend clean")
    ));
}

fn render_repos(report: &Report) {
    if report.repos.is_empty() {
        return;
    }
    ui::section(
        "git repos",
        Some(&format!(
            "{} found, {} without a commit in over {} days",
            report.repos.len(),
            report.stale.len(),
            report.stale_days
        )),
    );

    let rows: Vec<Vec<String>> = report
        .repos
        .iter()
        .take(12)
        .map(|repo| {
            let age = if repo.last_commit_ms > 0 {
                human_age(now_ms() - repo.last_commit_ms)
            } else {
                "no commits".to_string()
            };
            let state = if repo.dirty > 0 {
                ui::warn(&format!("{} changes", repo.dirty))
            } else {
                ui::grey("clean")
            };
            vec![
                ui::grey(&ui::pad_visible(&age, 8, false)),
                ui::bold(&repo.branch),
                state,
                tildify(&repo.path),
            ]
        })
        .collect();

    if !rows.is_empty() {
        ui::table(&["last commit", "branch", "state", "path"], &rows);
    }
    if report.repos.len() > rows.len() {
        ui::say_note(&format!(
            "  ... {} more repos",
            report.repos.len() - rows.len()
        ));
    }
}

fn render_links(report: &Report) {
    if report.links_total == 0 {
        return;
    }
    ui::section("broken links", Some(&report.links_total.to_string()));
    for link in report.broken_links.iter().take(12) {
        ui::bullet(&tildify(link), 2);
    }
    if report.links_total > report.broken_links.len() {
        ui::say_note(&format!(
            "  ... {} more links (only 40 paths retained)",
            report.links_total - report.broken_links.len()
        ));
    } else if report.broken_links.len() > 12 {
        ui::say_note(&format!(
            "  ... {} more links",
            report.broken_links.len() - 12
        ));
    }
}

fn relative(bytes: u64, max: u64) -> f64 {
    if max > 0 {
        bytes as f64 / max as f64
    } else {
        0.0
    }
}
