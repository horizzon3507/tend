use std::ffi::CString;
use std::fs::File;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

pub fn ensure_parent(path: &Path) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
}

pub fn expand_home(input: &str) -> PathBuf {
    if input == "~" {
        return home();
    }
    if let Some(rest) = input.strip_prefix("~/") {
        return home().join(rest);
    }
    PathBuf::from(input)
}

pub fn tildify(path: &Path) -> String {
    let home = home();
    match path.strip_prefix(&home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];

pub fn human_bytes(bytes: u64) -> String {
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    let text = if unit == 0 {
        format!("{}", value.round() as u64)
    } else {
        format!("{value:.1}")
    };
    format!("{} {}", text, UNITS[unit])
}

const MINUTE: i64 = 60_000;
const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;
const MONTH: i64 = 30 * DAY;
const YEAR: i64 = 365 * DAY;

pub fn human_age(ms: i64) -> String {
    if ms < MINUTE {
        return "now".to_string();
    }
    if ms < HOUR {
        return format!("{} min", ms / MINUTE);
    }
    if ms < DAY {
        return format!("{} h", ms / HOUR);
    }
    if ms < MONTH {
        return format!("{} d", ms / DAY);
    }
    if ms < YEAR {
        let months = ms / MONTH;
        return if months == 1 {
            "1 month".to_string()
        } else {
            format!("{months} months")
        };
    }
    let years = ms / YEAR;
    if years == 1 {
        "1 year".to_string()
    } else {
        format!("{years} years")
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn localtime(ms: i64) -> Option<libc::tm> {
    let seconds = (ms / 1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let result = unsafe { libc::localtime_r(&seconds, &mut tm) };
    if result.is_null() { None } else { Some(tm) }
}

/// Identifier for a backup, unique to the millisecond.
pub fn format_stamp(ms: i64) -> String {
    match localtime(ms) {
        Some(tm) => format!(
            "{:04}{:02}{:02}-{:02}{:02}{:02}{:03}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec,
            ms.rem_euclid(1000)
        ),
        None => format!("{ms}"),
    }
}

pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

pub struct Out {
    pub ok: bool,
    pub stdout: String,
}

/// Runs a program with a deadline. A hung child is killed instead of blocking the
/// tool forever, which matters when scanning dozens of unrelated repositories.
pub fn run(cmd: &str, args: &[String], timeout: Duration) -> Option<Out> {
    let mut child = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;

    let stdout = child.stdout.take()?;
    let stderr = child.stderr.take()?;
    let out_reader = std::thread::spawn(move || read_all(stdout));
    let err_reader = std::thread::spawn(move || read_all(stderr));

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(15));
            }
            Err(_) => break None,
        }
    };

    let stdout_text = out_reader.join().unwrap_or_default();
    let _ = err_reader.join();

    status.map(|status| Out {
        ok: status.success(),
        stdout: stdout_text,
    })
}

fn read_all(mut stream: impl Read) -> String {
    let mut buffer = Vec::new();
    let _ = stream.read_to_end(&mut buffer);
    String::from_utf8_lossy(&buffer).into_owned()
}

pub struct Disk {
    pub total: u64,
    pub used: u64,
    pub free: u64,
    pub mount: String,
}

pub fn disk(path: &Path) -> Option<Disk> {
    let raw = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(raw.as_ptr(), &mut stats) } != 0 {
        return None;
    }
    let fragment = stats.f_frsize as u64;
    if fragment == 0 {
        return None;
    }
    Some(Disk {
        total: stats.f_blocks as u64 * fragment,
        used: (stats.f_blocks as u64 - stats.f_bfree as u64) * fragment,
        free: stats.f_bavail as u64 * fragment,
        mount: mount_point(path),
    })
}

/// Longest mount point from /proc/mounts that contains the path.
fn mount_point(path: &Path) -> String {
    let Ok(table) = std::fs::read_to_string("/proc/mounts") else {
        return path.display().to_string();
    };
    let mut best: Option<String> = None;
    for line in table.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let Some(mount) = fields.get(1) else { continue };
        let mount = mount.replace("\\040", " ");
        let mount_path = Path::new(&mount);
        if path.starts_with(mount_path) {
            let longer = best.as_ref().map(|b| mount.len() > b.len()).unwrap_or(true);
            if longer {
                best = Some(mount);
            }
        }
    }
    best.unwrap_or_else(|| "/".to_string())
}

pub fn is_tty() -> bool {
    unsafe { libc::isatty(libc::STDOUT_FILENO) == 1 }
}

pub fn terminal_width() -> usize {
    if let Ok(columns) = std::env::var("COLUMNS") {
        if let Ok(parsed) = columns.parse::<usize>() {
            if parsed > 0 {
                return parsed;
            }
        }
    }
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } == 0;
    if ok && size.ws_col > 0 {
        size.ws_col as usize
    } else {
        80
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_scales_by_1024() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.0 KB");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(5 * 1024 * 1024 * 1024), "5.0 GB");
    }

    #[test]
    fn human_age_never_overflows_the_largest_unit() {
        assert_eq!(human_age(30_000), "now");
        assert_eq!(human_age(-1), "now");
        assert_eq!(human_age(5 * MINUTE), "5 min");
        assert_eq!(human_age(3 * HOUR), "3 h");
        assert_eq!(human_age(2 * DAY), "2 d");
        assert_eq!(human_age(45 * DAY), "1 month");
        assert_eq!(human_age(200 * DAY), "6 months");
        assert_eq!(human_age(3 * YEAR), "3 years");
        assert_eq!(human_age(1_970 * DAY), "5 years");
    }

    #[test]
    fn expand_home_handles_tilde() {
        let home = home();
        assert_eq!(expand_home("~"), home);
        assert_eq!(expand_home("~/x"), home.join("x"));
        assert_eq!(expand_home("/etc"), PathBuf::from("/etc"));
    }

    #[test]
    fn tildify_is_the_inverse_for_home_paths() {
        assert_eq!(tildify(&home()), "~");
        assert_eq!(tildify(&home().join("a/b")), "~/a/b");
        assert_eq!(tildify(Path::new("/etc/hosts")), "/etc/hosts");
    }

    #[test]
    fn run_reports_a_real_command() {
        let out = run(
            "sh",
            &["-c".into(), "printf oi".into()],
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(out.ok);
        assert_eq!(out.stdout, "oi");
    }

    #[test]
    fn run_kills_a_hung_command() {
        let started = Instant::now();
        let out = run("sleep", &["5".into()], Duration::from_millis(200));
        assert!(out.is_none());
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn disk_matches_statvfs_of_home() {
        let info = disk(&home()).expect("statvfs");
        assert!(info.total > 0);
        assert!(info.used > 0);
        assert!(info.free > 0);
        assert!(info.used <= info.total);
    }
}
