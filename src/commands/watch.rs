use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;
use serde_json::json;

use crate::cli::WatchArgs;
use crate::commands;
use crate::config;
use crate::monitor::{self, Sample};
use crate::ui;
use crate::util::{is_tty, tildify};

pub fn run(args: WatchArgs) -> Result<i32> {
    let config = config::load().config;
    let Some(roots) = commands::usable_roots(config::resolved_roots(&config, &args.common.roots))
    else {
        return Ok(1);
    };
    let mount = roots.first().cloned().unwrap_or_else(|| PathBuf::from("/"));

    let interval = args.interval.unwrap_or(1000).max(250);
    let count = if args.once {
        1
    } else {
        args.count.unwrap_or(0)
    };
    let json = args.common.json;
    let tty = is_tty() && !json;

    let running = Arc::new(AtomicBool::new(true));
    {
        let flag = Arc::clone(&running);
        let _ = ctrlc::set_handler(move || flag.store(false, Ordering::SeqCst));
    }

    if !json {
        ui::section("tend", Some(&format!("watch on {}", tildify(&mount))));
        let tail = if count > 0 {
            format!(" · {count} samples")
        } else {
            " · Ctrl+C to quit".to_string()
        };
        println!(
            "  {}{}\n",
            ui::grey(&format!("every {interval} ms")),
            ui::grey(&tail)
        );
    }

    let mut previous = monitor::cpu_snapshot();
    let mut samples: Vec<Sample> = Vec::new();
    let mut tick = 0u64;
    let mut painted = 0usize;

    loop {
        if !running.load(Ordering::SeqCst) {
            break;
        }
        sleep_interruptible(if tick == 0 { 250 } else { interval }, &running);
        if !running.load(Ordering::SeqCst) {
            break;
        }

        let current = monitor::sample(Some(&previous), &mount);
        previous = monitor::cpu_snapshot();
        samples.push(current);
        tick += 1;

        if json {
            if let Some(last) = samples.last() {
                println!(
                    "{}",
                    json!({
                        "at": last.at,
                        "cpu": last.cpu,
                        "memUsed": last.mem_used,
                        "memTotal": last.mem_total,
                        "disk": last.disk.as_ref().map(|disk| json!({
                            "total": disk.total,
                            "used": disk.used,
                            "free": disk.free,
                            "mount": disk.mount,
                        })),
                    })
                );
            }
        } else {
            let block = monitor::render(&samples);
            if tty {
                if painted > 0 {
                    print!("\x1b[{}A", painted);
                    for line in &block {
                        println!("\x1b[2K{line}");
                    }
                } else {
                    for line in &block {
                        println!("{line}");
                    }
                }
                painted = block.len();
                let _ = std::io::stdout().flush();
            } else {
                for line in &block {
                    println!("{line}");
                }
            }
        }

        if count > 0 && tick >= count {
            break;
        }
    }

    if tty {
        println!();
    }
    Ok(0)
}

/// Sleeps in short steps so Ctrl+C is noticed immediately instead of after a full
/// interval.
fn sleep_interruptible(total_ms: u64, running: &AtomicBool) {
    const STEP_MS: u64 = 50;
    let mut left = total_ms;
    while left > 0 && running.load(Ordering::SeqCst) {
        let chunk = left.min(STEP_MS);
        std::thread::sleep(Duration::from_millis(chunk));
        left -= chunk;
    }
}
