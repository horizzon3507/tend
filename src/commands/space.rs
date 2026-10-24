use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use serde_json::{Value, json};

use crate::cli::SpaceArgs;
use crate::scan;
use crate::ui;
use crate::util::{home, human_bytes, run as run_command, tildify};

const DOCKER_TIMEOUT: Duration = Duration::from_secs(10);
const CACHE_ITEMS: usize = 10;

struct CacheEntry {
    path: PathBuf,
    bytes: u64,
}

pub fn run(args: SpaceArgs) -> Result<i32> {
    let docker = docker_usage();
    let journald = journald_usage();
    let (cache, cache_total) = cache_usage();

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "docker": docker,
                "journald": journald,
                "cache": cache.iter().map(|entry| json!({
                    "bytes": entry.bytes,
                    "path": entry.path.display().to_string(),
                })).collect::<Vec<_>>(),
                "cacheTotal": cache_total,
            }))?
        );
        return Ok(0);
    }

    ui::section("tend", Some("space"));

    ui::section("docker", None);
    if docker.is_empty() {
        ui::say_note("docker is not available (or the daemon is not running)");
    } else {
        let rows: Vec<Vec<String>> = docker
            .iter()
            .map(|entry| {
                vec![
                    field(entry, "Type").unwrap_or("unknown").to_string(),
                    field(entry, "Size").unwrap_or("-").to_string(),
                    field(entry, "Reclaimable").unwrap_or("-").to_string(),
                ]
            })
            .collect();
        ui::table(&["type", "size", "reclaimable"], &rows);
    }

    ui::section("journald", None);
    match &journald {
        Some(text) => ui::say_note(text),
        None => ui::say_note("journalctl is not available"),
    }

    ui::section("user cache", None);
    if cache.is_empty() {
        ui::say_note("no cache directories found");
    } else {
        let rows: Vec<Vec<String>> = cache
            .iter()
            .map(|entry| vec![tildify(&entry.path), human_bytes(entry.bytes)])
            .collect();
        ui::table(&["path", "size"], &rows);
        ui::kv("total", &human_bytes(cache_total));
    }

    ui::blank();
    ui::say_note("read-only report · nothing was deleted");
    Ok(0)
}

fn field<'a>(entry: &'a Value, key: &str) -> Option<&'a str> {
    entry
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

fn docker_usage() -> Vec<Value> {
    let args = vec![
        "system".to_string(),
        "df".to_string(),
        "--format".to_string(),
        "{{json .}}".to_string(),
    ];
    let Some(out) = run_command("docker", &args, DOCKER_TIMEOUT) else {
        return Vec::new();
    };
    if !out.ok {
        return Vec::new();
    }
    parse_docker_lines(&out.stdout)
}

fn parse_docker_lines(text: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            serde_json::from_str(line).ok()
        })
        .collect()
}

fn journald_usage() -> Option<String> {
    let output = std::process::Command::new("journalctl")
        .arg("--disk-usage")
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !stdout.is_empty() {
        return Some(stdout);
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    (!stderr.is_empty()).then_some(stderr)
}

fn cache_usage() -> (Vec<CacheEntry>, u64) {
    let root = home().join(".cache");
    let Ok(entries) = fs::read_dir(&root) else {
        return (Vec::new(), 0);
    };

    let mut items: Vec<CacheEntry> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let meta = fs::symlink_metadata(&path).ok()?;
            if meta.file_type().is_symlink() || !meta.is_dir() {
                return None;
            }
            Some(CacheEntry {
                bytes: scan::subtree_bytes(&path),
                path,
            })
        })
        .collect();

    // The total covers every cache directory; only the listing is capped.
    let total = items.iter().map(|entry| entry.bytes).sum();
    items.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.path.cmp(&b.path)));
    items.truncate(CACHE_ITEMS);
    (items, total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_one_object_per_line() {
        let text = "{\"Type\":\"Images\",\"Size\":\"1GB\"}\n\n{\"Type\":\"Containers\",\"Size\":\"2GB\"}\n";
        let parsed = parse_docker_lines(text);
        assert_eq!(parsed.len(), 2);
        assert_eq!(field(&parsed[0], "Type"), Some("Images"));
        assert_eq!(field(&parsed[1], "Size"), Some("2GB"));
        assert_eq!(field(&parsed[0], "Reclaimable"), None);
    }

    #[test]
    fn skips_malformed_lines() {
        assert_eq!(
            parse_docker_lines("not json\n{\"Type\":\"Volumes\"}").len(),
            1
        );
        assert!(parse_docker_lines("   \n").is_empty());
    }
}
