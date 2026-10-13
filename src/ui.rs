use std::sync::atomic::{AtomicBool, Ordering};

use crate::util::{is_tty, terminal_width};

static COLOR: AtomicBool = AtomicBool::new(false);

pub fn init(no_color_flag: bool) {
    let disabled_env = std::env::var_os("NO_COLOR").is_some();
    let dumb_term = std::env::var("TERM").map(|t| t == "dumb").unwrap_or(false);
    COLOR.store(
        is_tty() && !disabled_env && !dumb_term && !no_color_flag,
        Ordering::Relaxed,
    );
}

fn coloring() -> bool {
    COLOR.load(Ordering::Relaxed)
}

fn paint(code: &str, text: &str) -> String {
    if coloring() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

pub fn brand(text: &str) -> String {
    paint("38;5;214", text)
}

pub fn good(text: &str) -> String {
    paint("38;5;114", text)
}

pub fn warn(text: &str) -> String {
    paint("38;5;214", text)
}

pub fn bad(text: &str) -> String {
    paint("38;5;203", text)
}

pub fn bold(text: &str) -> String {
    paint("1", text)
}

pub fn dim(text: &str) -> String {
    paint("2", text)
}

pub fn grey(text: &str) -> String {
    paint("38;5;245", text)
}

/// Green under 70%, amber under 90%, red above: the only place color carries meaning.
pub fn severity(ratio: f64, text: &str) -> String {
    if ratio >= 0.9 {
        bad(text)
    } else if ratio >= 0.7 {
        warn(text)
    } else {
        good(text)
    }
}

fn ratio_of(value: f64) -> f64 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

pub fn bar(ratio: f64, width: usize) -> String {
    let safe = ratio_of(ratio);
    let filled = ((safe * width as f64).round() as usize).min(width);
    let empty = width.saturating_sub(filled);
    format!(
        "{}{}",
        severity(safe, &"█".repeat(filled)),
        dim(&"░".repeat(empty))
    )
}

/// Neutral proportional meter for relative comparisons, where the ratio is not a
/// severity and must not be painted red.
pub fn meter(ratio: f64, width: usize) -> String {
    let safe = ratio_of(ratio);
    let mut filled = ((safe * width as f64).round() as usize).min(width);
    if safe > 0.0 && filled == 0 {
        filled = 1;
    }
    format!(
        "{}{}",
        brand(&"█".repeat(filled)),
        grey(&"░".repeat(width.saturating_sub(filled)))
    )
}

const BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

pub fn sparkline(values: &[f64]) -> String {
    values
        .iter()
        .map(|value| {
            let safe = ratio_of(*value);
            let index = ((safe * BLOCKS.len() as f64).floor() as usize).min(BLOCKS.len() - 1);
            BLOCKS[index]
        })
        .collect()
}

fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            for next in chars.by_ref() {
                if next == 'm' {
                    break;
                }
            }
            continue;
        }
        out.push(ch);
    }
    out
}

pub fn visible_width(text: &str) -> usize {
    strip_ansi(text).chars().count()
}

pub fn pad_visible(text: &str, width: usize, align_right: bool) -> String {
    let gap = width.saturating_sub(visible_width(text));
    let filler = " ".repeat(gap);
    if align_right {
        format!("{filler}{text}")
    } else {
        format!("{text}{filler}")
    }
}

fn width_cap() -> usize {
    terminal_width().clamp(48, 100)
}

pub fn section(title: &str, subtitle: Option<&str>) {
    let tail = match subtitle {
        Some(text) => format!(" {}", dim(text)),
        None => String::new(),
    };
    let title_width = visible_width(title) + visible_width(&tail);
    let rule_width = width_cap().saturating_sub(title_width + 4).max(8);
    println!("\n{} {}{}", brand("▌"), bold(title), tail);
    println!("{}", dim(&"─".repeat(rule_width)));
}

pub fn kv(label: &str, value: &str) {
    println!("  {}{}", grey(&pad_visible(label, 10, false)), value);
}

pub fn bullet(text: &str, indent: usize) {
    println!("{}· {text}", " ".repeat(indent));
}

pub fn blank() {
    println!();
}

pub fn say_note(text: &str) {
    println!("{}", grey(text));
}

pub fn say_info(text: &str) {
    println!("{} {text}", brand("i"));
}

pub fn say_ok(text: &str) {
    println!("{} {text}", good("✓"));
}

pub fn say_warn(text: &str) {
    println!("{} {text}", warn("▲"));
}

pub fn say_fail(text: &str) {
    println!("{} {text}", bad("✗"));
}

pub fn table(headers: &[&str], rows: &[Vec<String>]) {
    let mut widths: Vec<usize> = headers.iter().map(|h| visible_width(h)).collect();
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            if let Some(slot) = widths.get_mut(index) {
                *slot = (*slot).max(visible_width(cell));
            }
        }
    }

    let head = headers
        .iter()
        .enumerate()
        .map(|(index, header)| dim(&pad_visible(header, widths[index], false)))
        .collect::<Vec<_>>()
        .join("  ");
    println!("  {head}");

    for row in rows {
        let line = row
            .iter()
            .enumerate()
            .map(|(index, cell)| pad_visible(cell, widths.get(index).copied().unwrap_or(0), false))
            .collect::<Vec<_>>()
            .join("  ");
        println!("  {line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bars_keep_the_requested_width() {
        for ratio in [0.0, 0.34, 0.7, 0.91, 1.0] {
            assert_eq!(visible_width(&bar(ratio, 10)), 10);
            assert_eq!(visible_width(&meter(ratio, 16)), 16);
        }
        assert_eq!(visible_width(&bar(f64::NAN, 8)), 8);
        assert_eq!(visible_width(&meter(f64::NAN, 8)), 8);
    }

    #[test]
    fn a_small_nonzero_ratio_still_shows_one_block() {
        let painted = strip_ansi(&meter(0.001, 20));
        assert!(painted.starts_with('█'));
    }

    #[test]
    fn zero_ratio_is_all_empty() {
        let painted = strip_ansi(&meter(0.0, 6));
        assert_eq!(painted, "░░░░░░");
    }

    #[test]
    fn sparkline_has_one_glyph_per_sample() {
        assert_eq!(sparkline(&[]), "");
        assert_eq!(sparkline(&[0.0, 0.5, 1.0]).chars().count(), 3);
        assert_eq!(sparkline(&[0.0]).chars().count(), 1);
    }

    #[test]
    fn padding_aligns_on_visible_text() {
        assert_eq!(pad_visible("abc", 6, false), "abc   ");
        assert_eq!(pad_visible("abc", 6, true), "   abc");
        assert_eq!(visible_width(&pad_visible("abc", 2, false)), 3);
    }

    #[test]
    fn visible_width_ignores_ansi() {
        let colored = format!("{}x{}", brand("ab"), dim("cd"));
        assert_eq!(visible_width(&colored), 5);
    }
}
