use anyhow::Result;
use serde_json::json;

use crate::cli::StatusArgs;
use crate::config;
use crate::scan::{ScanOptions, build_report};
use crate::ui;
use crate::util::{human_age, human_bytes, now_ms, tildify};
use crate::{backup, commands};

pub fn run(args: StatusArgs) -> Result<i32> {
    let config = config::load().config;
    let Some(roots) = commands::usable_roots(config::resolved_roots(&config, &args.common.roots))
    else {
        return Ok(1);
    };

    let depth = args.common.depth.unwrap_or(config.max_depth).min(3);
    let report = build_report(&ScanOptions {
        roots: roots.clone(),
        depth,
        stale_days: config.stale_days,
        safe_only: true,
        limit: 120,
        repos: false,
        links: true,
        measure_dirs: true,
    });

    let backups = backup::list_backups(&config::backup_dest(&config, None));
    let last = backups.first();

    if args.common.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "disk": report.disk.as_ref().map(|disk| json!({
                    "total": disk.total,
                    "used": disk.used,
                    "free": disk.free,
                    "mount": disk.mount,
                })),
                "biggest": report.biggest.iter().map(|entry| json!({
                    "path": entry.path.display().to_string(),
                    "bytes": entry.bytes,
                })).collect::<Vec<_>>(),
                "reclaimable": report.reclaimable_safe,
                "targets": report.findings.len(),
                "brokenLinks": report.broken_links.len(),
                "protectedPaths": report.protected_paths.len(),
                "backups": backups.len(),
                "lastBackup": last.map(|manifest| manifest.created_at),
                "configPath": config::config_path().display().to_string(),
                "dataDir": config::data_dir().display().to_string(),
            }))?
        );
        return Ok(0);
    }

    let label = roots
        .iter()
        .map(|root| tildify(root))
        .collect::<Vec<_>>()
        .join(", ");
    ui::section("tend", Some(&label));

    if let Some(disk) = &report.disk {
        let ratio = if disk.total > 0 {
            disk.used as f64 / disk.total as f64
        } else {
            0.0
        };
        ui::kv(
            "disk",
            &format!(
                "{} {}",
                ui::severity(ratio, &ui::pad_visible(&percent(ratio), 4, false)),
                ui::dim(&format!(
                    "{} of {}, {} free",
                    human_bytes(disk.used),
                    human_bytes(disk.total),
                    human_bytes(disk.free)
                )),
            ),
        );
    }

    let reclaim = human_bytes(report.reclaimable_safe);
    let reclaim_text = if report.reclaimable_safe > 0 {
        ui::brand(&reclaim)
    } else {
        ui::grey(&reclaim)
    };
    ui::kv(
        "cleanup",
        &format!(
            "{} {}",
            reclaim_text,
            ui::dim(&format!("in {} directories", report.findings.len()))
        ),
    );

    let backup_text = match last {
        Some(manifest) => format!(
            "{} ago {}",
            human_age(now_ms() - manifest.created_at),
            ui::dim(&format!("({} kept)", backups.len()))
        ),
        None => ui::warn("none yet"),
    };
    ui::kv("backup", &backup_text);

    let link_text = format!("{} broken", report.broken_links.len());
    ui::kv(
        "links",
        &if report.broken_links.is_empty() {
            ui::grey(&link_text)
        } else {
            ui::warn(&link_text)
        },
    );

    if !report.biggest.is_empty() {
        ui::blank();
        let max = report.biggest.first().map(|entry| entry.bytes).unwrap_or(1);
        for entry in report.biggest.iter().take(4) {
            let relative = ratio(entry.bytes, max);
            println!(
                "  {}  {}  {}",
                ui::bold(&ui::pad_visible(&human_bytes(entry.bytes), 9, true)),
                ui::meter(relative, 12),
                tildify(&entry.path)
            );
        }
    }

    ui::blank();
    if report.reclaimable_safe > 0 {
        ui::say_note(&format!(
            "  biggest win: {} reclaims {}",
            ui::bold("tend clean"),
            human_bytes(report.reclaimable_safe)
        ));
    }
    if last.is_none() {
        ui::say_warn(&format!(
            "no dotfile backup yet, run {}",
            ui::bold("tend backup")
        ));
    }

    Ok(0)
}

fn percent(ratio: f64) -> String {
    format!("{}%", (ratio * 100.0).round())
}

fn ratio(bytes: u64, max: u64) -> f64 {
    if max > 0 {
        bytes as f64 / max as f64
    } else {
        0.0
    }
}
