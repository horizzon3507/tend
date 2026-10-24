use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config;
use crate::util::{ensure_parent, now_ms};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub at: i64,
    pub command: String,
    pub roots: Vec<String>,
    pub removed: usize,
    pub freed: u64,
    pub errors: usize,
}

pub fn path() -> PathBuf {
    config::data_dir().join("history.jsonl")
}

/// Appends one cleanup result. History is best effort: a read-only data directory
/// must never turn a successful cleanup into a failure.
pub fn record(command: &str, roots: &[PathBuf], removed: usize, freed: u64, errors: usize) {
    let entry = Entry {
        at: now_ms(),
        command: command.to_string(),
        roots: roots
            .iter()
            .map(|root| root.display().to_string())
            .collect(),
        removed,
        freed,
        errors,
    };
    let Ok(text) = serde_json::to_string(&entry) else {
        return;
    };
    let path = path();
    ensure_parent(&path);
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(file, "{text}");
    }
}

/// Newest first. A malformed line is skipped instead of breaking the whole list.
pub fn read(limit: usize) -> Vec<Entry> {
    let Ok(text) = std::fs::read_to_string(path()) else {
        return Vec::new();
    };
    let mut entries: Vec<Entry> = text
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    if limit > 0 && entries.len() > limit {
        entries = entries.split_off(entries.len() - limit);
    }
    entries.reverse();
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_line_and_keeps_newest_first() {
        let entries = [
            Entry {
                at: 1,
                command: "clean".into(),
                roots: vec![],
                removed: 1,
                freed: 10,
                errors: 0,
            },
            Entry {
                at: 2,
                command: "projects".into(),
                roots: vec![],
                removed: 2,
                freed: 20,
                errors: 0,
            },
        ];
        let text: String = entries
            .iter()
            .map(|entry| format!("{}\n", serde_json::to_string(entry).unwrap()))
            .collect();
        let parsed: Vec<Entry> = text
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[1].command, "projects");
    }
}
