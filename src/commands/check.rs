use anyhow::Result;
use serde_json::json;

use crate::cli::CheckArgs;
use crate::commands;
use crate::config;
use crate::ui;
use crate::util::{disk, human_bytes};

pub fn run(args: CheckArgs) -> Result<i32> {
    let config = config::load().config;
    let Some(roots) = commands::usable_roots(config::resolved_roots(&config, &args.common.roots))
    else {
        return Ok(1);
    };

    // statvfs on the first root is enough: the question is about the disk, not the tree.
    let Some(disk) = disk(&roots[0]) else {
        if !args.quiet && !args.common.json {
            ui::section("tend", Some("check"));
            ui::say_fail("disk usage is unavailable for this root");
        }
        return Ok(1);
    };

    let ratio = if disk.total > 0 {
        disk.used as f64 / disk.total as f64
    } else {
        0.0
    };
    let percent = ratio * 100.0;
    let over = percent >= args.max_usage as f64;
    let code = if over { 2 } else { 0 };

    if args.quiet {
        return Ok(code);
    }

    if args.common.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": !over,
                "usagePercent": round_percent(percent),
                "maxUsage": args.max_usage,
                "used": disk.used,
                "total": disk.total,
                "free": disk.free,
                "mount": disk.mount,
                "over": over,
            }))?
        );
        return Ok(code);
    }

    ui::section("tend", Some("check"));
    ui::kv("disk", &ui::bar(ratio, 24));
    ui::kv(
        "usage",
        &format!(
            "{} of {} · threshold {}%",
            ui::severity(ratio, &format!("{}%", percent.round())),
            human_bytes(disk.total),
            args.max_usage
        ),
    );
    ui::blank();
    if over {
        ui::say_fail(&format!(
            "usage is at or above the {}% threshold",
            args.max_usage
        ));
    } else {
        ui::say_ok(&format!("usage is below the {}% threshold", args.max_usage));
    }
    Ok(code)
}

fn round_percent(percent: f64) -> f64 {
    (percent * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounds_usage_to_one_decimal() {
        assert_eq!(round_percent(93.44), 93.4);
        assert_eq!(round_percent(93.46), 93.5);
        assert_eq!(round_percent(0.0), 0.0);
        assert_eq!(round_percent(100.0), 100.0);
    }
}
