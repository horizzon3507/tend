use std::fs;
use std::path::Path;

use crate::ui;
use crate::util::{Disk, disk, human_bytes, now_ms};

pub struct CpuSnapshot {
    idle: u64,
    total: u64,
}

pub struct Sample {
    pub at: i64,
    pub cpu: f64,
    pub mem_used: u64,
    pub mem_total: u64,
    pub disk: Option<Disk>,
}

pub fn cpu_snapshot() -> CpuSnapshot {
    let text = fs::read_to_string("/proc/stat").unwrap_or_default();
    let line = text.lines().next().unwrap_or("").trim();
    let values: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|field| field.parse::<u64>().ok())
        .collect();

    let idle = values.get(3).copied().unwrap_or(0) + values.get(4).copied().unwrap_or(0);
    let total = values.iter().sum();
    CpuSnapshot { idle, total }
}

fn mem_snapshot() -> (u64, u64) {
    let text = fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let read = |key: &str| -> u64 {
        text.lines()
            .find_map(|line| {
                let (name, rest) = line.split_once(':')?;
                if name != key {
                    return None;
                }
                let value: u64 = rest.split_whitespace().next()?.parse().ok()?;
                Some(value * 1024)
            })
            .unwrap_or(0)
    };

    let total = read("MemTotal");
    let available = read("MemAvailable");
    (total.saturating_sub(available), total)
}

pub fn sample(previous: Option<&CpuSnapshot>, mount: &Path) -> Sample {
    let current = cpu_snapshot();
    let mut cpu = 0.0;
    if let Some(previous) = previous {
        let total_delta = current.total.saturating_sub(previous.total);
        let idle_delta = current.idle.saturating_sub(previous.idle);
        if total_delta > 0 {
            cpu = (1.0 - idle_delta as f64 / total_delta as f64).clamp(0.0, 1.0);
        }
    }

    let (mem_used, mem_total) = mem_snapshot();
    Sample {
        at: now_ms(),
        cpu,
        mem_used,
        mem_total,
        disk: disk(mount),
    }
}

pub fn render(samples: &[Sample]) -> Vec<String> {
    let Some(current) = samples.last() else {
        return Vec::new();
    };
    let history: Vec<&Sample> = samples.iter().rev().take(40).collect();
    let history: Vec<&Sample> = history.into_iter().rev().collect();

    let cpu_ratio = current.cpu.clamp(0.0, 1.0);
    let cpu_line = format!(
        "  {}{} {}",
        ui::grey(&ui::pad_visible("cpu", 6, false)),
        ui::severity(
            cpu_ratio,
            &ui::pad_visible(&format!("{}%", (cpu_ratio * 100.0).round()), 4, false)
        ),
        ui::sparkline(&history.iter().map(|sample| sample.cpu).collect::<Vec<_>>()),
    );

    let mem_ratio = if current.mem_total > 0 {
        current.mem_used as f64 / current.mem_total as f64
    } else {
        0.0
    };
    let mem_line = format!(
        "  {}{} {}  {}",
        ui::grey(&ui::pad_visible("mem", 6, false)),
        ui::severity(
            mem_ratio,
            &ui::pad_visible(&format!("{}%", (mem_ratio * 100.0).round()), 4, false)
        ),
        ui::dim(&format!(
            "{} of {}",
            human_bytes(current.mem_used),
            human_bytes(current.mem_total)
        )),
        ui::sparkline(
            &history
                .iter()
                .map(|sample| if sample.mem_total > 0 {
                    sample.mem_used as f64 / sample.mem_total as f64
                } else {
                    0.0
                })
                .collect::<Vec<_>>()
        ),
    );

    let mut lines = vec![cpu_line, mem_line];

    if let Some(disk) = &current.disk {
        let ratio = if disk.total > 0 {
            disk.used as f64 / disk.total as f64
        } else {
            0.0
        };
        lines.push(format!(
            "  {}{} {} {}",
            ui::grey(&ui::pad_visible("disk", 6, false)),
            ui::bar(ratio, 20),
            ui::severity(ratio, &format!("{}%", (ratio * 100.0).round())),
            ui::dim(&format!("{} free", human_bytes(disk.free))),
        ));
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_snapshot_reads_the_first_proc_stat_line() {
        let snapshot = cpu_snapshot();
        assert!(snapshot.total > 0);
        assert!(snapshot.idle <= snapshot.total);
    }

    #[test]
    fn memory_totals_look_like_a_real_machine() {
        let (used, total) = mem_snapshot();
        assert!(total > 0);
        assert!(used <= total);
    }

    #[test]
    fn a_sample_without_history_has_no_cpu_signal() {
        let sample = sample(None, Path::new("/"));
        assert_eq!(sample.cpu, 0.0);
        assert!(sample.mem_total > 0);
        assert!(sample.disk.is_some());
    }

    #[test]
    fn render_produces_one_line_per_metric() {
        let samples = vec![sample(None, Path::new("/"))];
        let lines = render(&samples);
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("cpu"));
        assert!(lines[1].contains("mem"));
        assert!(lines[2].contains("disk"));
    }

    #[test]
    fn render_of_an_empty_history_is_empty() {
        assert!(render(&[]).is_empty());
    }
}
