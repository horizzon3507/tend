use std::path::Path;

use anyhow::Result;
use serde_json::json;

use crate::backup as engine;
use crate::cli::{BackupAction, BackupArgs};
use crate::config;
use crate::ui;
use crate::util::{human_age, human_bytes, now_ms, tildify};

pub fn run(args: BackupArgs) -> Result<i32> {
    let loaded = config::load();
    let config = loaded.config;
    let dest = config::backup_dest(&config, args.dest.as_deref());
    let keep = args.keep.unwrap_or(config.backup.keep);

    if loaded.created && !args.json {
        ui::say_note(&format!(
            "config created at {}",
            tildify(&config::config_path())
        ));
    }

    match args.action.as_ref() {
        Some(BackupAction::List) => list(&dest, args.json),
        Some(BackupAction::Restore { reference }) => restore(&args, &dest, reference.clone()),
        _ => create(&args, &config, &dest, keep),
    }
}

fn create(args: &BackupArgs, config: &config::Config, dest: &Path, keep: usize) -> Result<i32> {
    let paths = if args.path.is_empty() {
        config.backup.paths.clone()
    } else {
        args.path.clone()
    };

    let result = match engine::create(&engine::CreateOptions {
        dest,
        paths: &paths,
        keep,
        force: args.force,
        size_limit: engine::SIZE_GUARD,
    }) {
        Ok(result) => result,
        Err(error) => {
            ui::say_fail(&error.to_string());
            return Ok(1);
        }
    };

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "manifest": result.manifest,
                "rotated": result.rotated.iter().map(|path| path.display().to_string()).collect::<Vec<_>>(),
            }))?
        );
        return Ok(0);
    }

    let manifest = &result.manifest;
    ui::section("tend", Some("backup"));
    ui::kv("archive", &tildify(Path::new(&manifest.archive)));
    ui::kv("size", &human_bytes(manifest.bytes));
    ui::kv("files", &manifest.file_count.to_string());
    ui::kv(
        "sha256",
        &ui::grey(manifest.sha256.as_deref().unwrap_or("unavailable")),
    );
    ui::kv("paths", &manifest.paths.len().to_string());
    for entry in &manifest.paths {
        ui::bullet(&ui::grey(&tildify(Path::new(entry))), 2);
    }
    if !result.rotated.is_empty() {
        ui::say_warn(&format!(
            "{} old backups removed (keeping {keep})",
            result.rotated.len()
        ));
    }
    Ok(0)
}

fn list(dest: &Path, json: bool) -> Result<i32> {
    let all = engine::list_backups(dest);
    if json {
        println!("{}", serde_json::to_string_pretty(&all)?);
        return Ok(0);
    }

    ui::section("tend", Some(&format!("backups in {}", tildify(dest))));
    if all.is_empty() {
        ui::say_note("  no backups yet, run: tend backup");
        return Ok(0);
    }

    let rows: Vec<Vec<String>> = all
        .iter()
        .map(|manifest| {
            vec![
                ui::bold(&manifest.id),
                ui::grey(&ui::pad_visible(
                    &human_age(now_ms() - manifest.created_at),
                    8,
                    false,
                )),
                human_bytes(manifest.bytes),
                if manifest.file_count > 0 {
                    manifest.file_count.to_string()
                } else {
                    ui::grey("?")
                },
                ui::grey(&tildify(Path::new(&manifest.archive))),
            ]
        })
        .collect();
    ui::table(&["id", "age", "size", "files", "archive"], &rows);
    Ok(0)
}

fn restore(args: &BackupArgs, dest: &Path, reference: Option<String>) -> Result<i32> {
    let reference = reference.unwrap_or_else(|| "latest".to_string());
    let Some(manifest) = engine::resolve_backup(dest, &reference) else {
        ui::say_fail(&format!(
            "backup \"{reference}\" not found in {}",
            tildify(dest)
        ));
        return Ok(1);
    };

    let (target, entries) =
        match engine::restore(&manifest, args.to.as_deref(), args.in_place, args.dry_run) {
            Ok(pair) => pair,
            Err(error) => {
                ui::say_fail(&error.to_string());
                return Ok(1);
            }
        };

    if args.json {
        println!(
            "{}",
            json!({
                "manifest": manifest.id,
                "target": target,
                "entries": entries,
                "dryRun": args.dry_run,
            })
        );
        return Ok(0);
    }

    ui::section("tend", Some(&format!("restore {}", manifest.id)));
    ui::kv("archive", &tildify(Path::new(&manifest.archive)));
    ui::kv("files", &entries.to_string());
    ui::kv(
        "target",
        &if args.in_place {
            ui::bad("/ (overwrites current files)")
        } else {
            tildify(Path::new(&target))
        },
    );

    if args.dry_run {
        ui::blank();
        for entry in engine::archive_entries(Path::new(&manifest.archive), 15)? {
            ui::bullet(&ui::grey(&entry), 2);
        }
        ui::say_info("dry run, nothing was written");
        return Ok(0);
    }

    ui::blank();
    ui::say_ok(&format!(
        "restored to {}",
        ui::bold(&tildify(Path::new(&target)))
    ));
    if !args.in_place {
        ui::say_note("  check the contents before copying over the originals");
    }
    Ok(0)
}
