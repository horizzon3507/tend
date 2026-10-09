use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::{Value, json};

use crate::targets::{self, Target};
use crate::util::{Disk, disk, now_ms, run};

/// du counts allocated blocks, not apparent size, so a tree full of tiny files
/// reports what it really occupies on disk.
fn usage(meta: &fs::Metadata) -> u64 {
    meta.blocks() * 512
}

pub struct Finding {
    pub path: PathBuf,
    pub bytes: u64,
    pub target: &'static Target,
}

pub struct DirSize {
    pub path: PathBuf,
    pub bytes: u64,
}

pub struct Repo {
    pub path: PathBuf,
    pub last_commit_ms: i64,
    pub dirty: usize,
    pub branch: String,
}

pub struct Report {
    pub roots: Vec<PathBuf>,
    pub disk: Option<Disk>,
    pub biggest: Vec<DirSize>,
    pub findings: Vec<Finding>,
    pub reclaimable: u64,
    pub reclaimable_safe: u64,
    pub repos: Vec<Repo>,
    pub stale: Vec<Repo>,
    pub broken_links: Vec<PathBuf>,
    pub truncated: bool,
    pub protected_paths: Vec<PathBuf>,
    pub stale_days: i64,
}

pub struct ScanOptions {
    pub roots: Vec<PathBuf>,
    pub depth: usize,
    pub stale_days: i64,
    pub safe_only: bool,
    pub limit: usize,
    pub repos: bool,
    pub links: bool,
    /// Measuring the whole tree is only needed for the "biggest directories" view.
    /// Without it the walk behaves like find: it stats directories, not every file.
    pub measure_dirs: bool,
}

const LINK_LIMIT: usize = 40;

struct Walker {
    max_depth: usize,
    safe_only: bool,
    measure_dirs: bool,
    want_repos: bool,
    want_links: bool,
    dirs: Vec<DirSize>,
    targets: Vec<(PathBuf, u64)>,
    protected: Vec<PathBuf>,
    links: Vec<PathBuf>,
    repos: Vec<PathBuf>,
    /// Hard links share their blocks; du counts each inode once and so does this.
    seen: HashSet<(u64, u64)>,
    root_dev: u64,
}

impl Walker {
    fn new(options: &ScanOptions) -> Self {
        Walker {
            max_depth: options.depth,
            safe_only: options.safe_only,
            measure_dirs: options.measure_dirs,
            want_repos: options.repos,
            want_links: options.links,
            dirs: Vec::new(),
            targets: Vec::new(),
            protected: Vec::new(),
            links: Vec::new(),
            repos: Vec::new(),
            seen: HashSet::new(),
            root_dev: 0,
        }
    }

    fn matches_target(&self, name: &str) -> bool {
        match targets::find(name) {
            Some(target) => !self.safe_only || target.safe,
            None => false,
        }
    }

    /// Returns the total block usage of `dir` and records the findings found on the
    /// way. Target directories are pruned from further matching but still measured.
    /// `pruned` marks a subtree where a `.git` is build output, not a project.
    fn walk(&mut self, dir: &Path, depth: usize, inside_target: bool, pruned: bool) -> u64 {
        let measuring = self.measure_dirs || inside_target;
        let mut total = if measuring {
            match fs::symlink_metadata(dir) {
                Ok(meta) => usage(&meta),
                Err(_) => return 0,
            }
        } else {
            0
        };

        let Ok(entries) = fs::read_dir(dir) else {
            return total;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let child_depth = depth + 1;

            if file_type.is_symlink() {
                if measuring {
                    let Ok(meta) = entry.metadata() else {
                        continue;
                    };
                    if meta.dev() != self.root_dev {
                        continue;
                    }
                    total += usage(&meta);
                }
                if self.want_links
                    && child_depth <= self.max_depth
                    && self.links.len() < LINK_LIMIT
                    && fs::metadata(&path).is_err()
                {
                    self.links.push(path);
                }
                continue;
            }

            if file_type.is_dir() {
                let Ok(meta) = entry.metadata() else {
                    continue;
                };
                if meta.dev() != self.root_dev {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                let is_target =
                    !inside_target && child_depth <= self.max_depth && self.matches_target(&name);

                if self.want_repos && name == ".git" && child_depth <= self.max_depth && !pruned {
                    if let Some(parent) = path.parent() {
                        self.repos.push(parent.to_path_buf());
                    }
                }

                let child_pruned = pruned || is_target || name == ".cache";
                let bytes = if is_target {
                    let measured = self.walk(&path, child_depth, true, child_pruned);
                    if targets::is_protected(&path) {
                        self.protected.push(path.clone());
                    } else {
                        self.targets.push((path.clone(), measured));
                    }
                    measured
                } else if measuring || child_depth <= self.max_depth {
                    self.walk(&path, child_depth, inside_target, child_pruned)
                } else {
                    0
                };

                total += bytes;
                if self.measure_dirs && child_depth <= self.max_depth {
                    self.dirs.push(DirSize { path, bytes });
                }
                continue;
            }

            if measuring {
                let Ok(meta) = entry.metadata() else {
                    continue;
                };
                if meta.dev() != self.root_dev {
                    continue;
                }
                if meta.nlink() > 1 && !self.seen.insert((meta.dev(), meta.ino())) {
                    continue;
                }
                total += usage(&meta);
            }
        }

        total
    }
}

/// Sizes for the top-level directories, dropping any entry already explained by an
/// ancestor that is also in the list.
fn biggest_dirs(mut dirs: Vec<DirSize>, top: usize) -> Vec<DirSize> {
    dirs.sort_by(|a, b| {
        b.bytes.cmp(&a.bytes).then_with(|| {
            a.path
                .components()
                .count()
                .cmp(&b.path.components().count())
        })
    });

    let mut kept: Vec<DirSize> = Vec::new();
    for entry in dirs {
        let nested = kept
            .iter()
            .any(|parent| entry.path.starts_with(&parent.path));
        if nested {
            continue;
        }
        kept.push(entry);
        if kept.len() >= top {
            break;
        }
    }
    kept
}

pub fn subtree_bytes(path: &Path) -> u64 {
    let mut seen = HashSet::new();
    subtree_with_seen(path, &mut seen)
}

fn subtree_with_seen(path: &Path, seen: &mut HashSet<(u64, u64)>) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    let mut total = usage(&meta);
    if !meta.is_dir() {
        return total;
    }
    let Ok(entries) = fs::read_dir(path) else {
        return total;
    };
    for entry in entries.flatten() {
        let child = entry.path();
        if let Ok(child_meta) = entry.metadata() {
            if child_meta.nlink() > 1
                && !child_meta.is_dir()
                && !seen.insert((child_meta.dev(), child_meta.ino()))
            {
                continue;
            }
        }
        total += subtree_with_seen(&child, seen);
    }
    total
}

fn inspect_repo(path: &Path) -> Option<Repo> {
    let repo = path.display().to_string();
    let log = run(
        "git",
        &[
            "-C".into(),
            repo.clone(),
            "log".into(),
            "-1".into(),
            "--format=%ct".into(),
        ],
        Duration::from_secs(8),
    )?;
    if !log.ok {
        return None;
    }
    let seconds: f64 = log.stdout.trim().parse().ok()?;

    let status = run(
        "git",
        &[
            "-C".into(),
            repo,
            "status".into(),
            "--porcelain=v1".into(),
            "--branch".into(),
        ],
        Duration::from_secs(15),
    );

    let mut branch = "?".to_string();
    let mut dirty = 0usize;
    if let Some(status) = status {
        for line in status.stdout.lines().filter(|line| !line.is_empty()) {
            if let Some(rest) = line.strip_prefix("## ") {
                branch = rest
                    .split(['.', ' ', '\n'])
                    .next()
                    .unwrap_or("?")
                    .to_string();
            } else {
                dirty += 1;
            }
        }
    }

    Some(Repo {
        path: path.to_path_buf(),
        last_commit_ms: if seconds.is_finite() {
            (seconds * 1000.0) as i64
        } else {
            0
        },
        dirty,
        branch,
    })
}

fn scan_repos(paths: Vec<PathBuf>) -> Vec<Repo> {
    let total = paths.len();
    if total == 0 {
        return Vec::new();
    }
    let workers = 6.min(total);
    let cursor = AtomicUsize::new(0);
    let collected: Mutex<Vec<Repo>> = Mutex::new(Vec::new());

    let paths = &paths;
    let cursor = &cursor;
    let collected = &collected;
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(move || {
                loop {
                    let index = cursor.fetch_add(1, Ordering::Relaxed);
                    let Some(path) = paths.get(index) else {
                        break;
                    };
                    if let Some(repo) = inspect_repo(path) {
                        if let Ok(mut guard) = collected.lock() {
                            guard.push(repo);
                        }
                    }
                }
            });
        }
    });

    let mut repos = {
        let mut guard = collected.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *guard)
    };
    repos.sort_by_key(|repo| repo.last_commit_ms);
    repos
}

pub fn build_report(options: &ScanOptions) -> Report {
    let mut walker = Walker::new(options);

    for root in &options.roots {
        walker.root_dev = fs::symlink_metadata(root)
            .map(|meta| meta.dev())
            .unwrap_or(0);
        walker.walk(root, 0, false, false);
    }

    let mut findings: Vec<Finding> = walker
        .targets
        .into_iter()
        .filter_map(|(path, bytes)| {
            let target = targets::find(&path.file_name()?.to_string_lossy())?;
            Some(Finding {
                path,
                bytes,
                target,
            })
        })
        .collect();
    findings.sort_by_key(|finding| std::cmp::Reverse(finding.bytes));
    let truncated = findings.len() > options.limit;
    findings.truncate(options.limit);

    let reclaimable = findings.iter().map(|f| f.bytes).sum();
    let reclaimable_safe = findings
        .iter()
        .filter(|f| f.target.safe)
        .map(|f| f.bytes)
        .sum();

    let repos = if options.repos {
        let mut unique = walker.repos;
        unique.sort();
        unique.dedup();
        scan_repos(unique)
    } else {
        Vec::new()
    };

    let cutoff = now_ms() - options.stale_days * 86_400_000;
    let stale: Vec<Repo> = repos
        .iter()
        .filter(|repo| repo.last_commit_ms > 0 && repo.last_commit_ms < cutoff)
        .map(|repo| Repo {
            path: repo.path.clone(),
            last_commit_ms: repo.last_commit_ms,
            dirty: repo.dirty,
            branch: repo.branch.clone(),
        })
        .collect();

    let mut protected_paths = walker.protected;
    protected_paths.sort();
    protected_paths.dedup();

    let mut broken_links = if options.links {
        walker.links
    } else {
        Vec::new()
    };
    broken_links.sort();
    broken_links.dedup();

    let biggest = biggest_dirs(walker.dirs, 8);
    let disk = options.roots.first().and_then(|root| disk(root));

    Report {
        roots: options.roots.clone(),
        disk,
        biggest,
        findings,
        reclaimable,
        reclaimable_safe,
        repos,
        stale,
        broken_links,
        truncated,
        protected_paths,
        stale_days: options.stale_days,
    }
}

impl Report {
    pub fn findings_json(&self) -> Value {
        Value::Array(
            self.findings
                .iter()
                .map(|finding| {
                    json!({
                        "path": finding.path.display().to_string(),
                        "bytes": finding.bytes,
                        "safe": finding.target.safe,
                        "label": finding.target.label,
                    })
                })
                .collect(),
        )
    }

    pub fn to_json(&self) -> Value {
        json!({
            "roots": self.roots.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
            "generatedAt": now_ms(),
            "disk": self.disk.as_ref().map(|disk| json!({
                "total": disk.total,
                "used": disk.used,
                "free": disk.free,
                "mount": disk.mount,
            })),
            "biggest": self.biggest.iter().map(|entry| json!({
                "path": entry.path.display().to_string(),
                "bytes": entry.bytes,
            })).collect::<Vec<_>>(),
            "findings": self.findings_json(),
            "reclaimable": self.reclaimable,
            "reclaimableSafe": self.reclaimable_safe,
            "repos": self.repos.iter().map(|repo| json!({
                "path": repo.path.display().to_string(),
                "lastCommitMs": repo.last_commit_ms,
                "dirty": repo.dirty,
                "branch": repo.branch,
            })).collect::<Vec<_>>(),
            "stale": self.stale.iter().map(|repo| json!({
                "path": repo.path.display().to_string(),
                "lastCommitMs": repo.last_commit_ms,
                "dirty": repo.dirty,
                "branch": repo.branch,
            })).collect::<Vec<_>>(),
            "brokenLinks": self.broken_links.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
            "truncated": self.truncated,
            "protectedPaths": self.protected_paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
            "staleDays": self.stale_days,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::create_dir_all(root.join("proj/node_modules/pkg")).unwrap();
        fs::write(
            root.join("proj/node_modules/pkg/index.js"),
            "x".repeat(8192),
        )
        .unwrap();
        fs::create_dir_all(root.join("proj/src")).unwrap();
        fs::write(root.join("proj/src/main.rs"), "fn main() {}").unwrap();
        fs::create_dir_all(root.join("proj/target/debug")).unwrap();
        fs::write(root.join("proj/target/debug/bin"), "binario").unwrap();
        fs::create_dir_all(root.join("deep/a/b/.venv")).unwrap();
        fs::write(root.join("deep/a/b/.venv/pyvenv.cfg"), "home = /usr").unwrap();
        std::os::unix::fs::symlink(root.join("missing-target"), root.join("broken-link")).unwrap();
        dir
    }

    fn options(root: &Path, safe_only: bool) -> ScanOptions {
        ScanOptions {
            roots: vec![root.to_path_buf()],
            depth: 6,
            stale_days: 180,
            safe_only,
            limit: 400,
            repos: false,
            links: true,
            measure_dirs: true,
        }
    }

    #[test]
    fn finds_safe_targets_and_ignores_unsafe_ones() {
        let dir = sandbox();
        let report = build_report(&options(dir.path(), true));
        let names: Vec<String> = report
            .findings
            .iter()
            .map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"node_modules".to_string()));
        assert!(names.contains(&"target".to_string()));
        assert!(!names.contains(&".venv".to_string()));
    }

    #[test]
    fn includes_unsafe_targets_when_asked() {
        let dir = sandbox();
        let report = build_report(&options(dir.path(), false));
        let names: Vec<String> = report
            .findings
            .iter()
            .map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&".venv".to_string()));
    }

    #[test]
    fn findings_are_sorted_by_size_and_counted_once() {
        let dir = sandbox();
        let report = build_report(&options(dir.path(), false));
        assert!(report.findings.windows(2).all(|w| w[0].bytes >= w[1].bytes));
        assert_eq!(
            report.reclaimable,
            report.findings.iter().map(|f| f.bytes).sum::<u64>()
        );
        assert!(report.reclaimable_safe < report.reclaimable);
    }

    #[test]
    fn nested_targets_are_not_counted_twice() {
        let dir = sandbox();
        let nested = dir.path().join("proj/node_modules/outro/node_modules");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("x.js"), "y".repeat(4096)).unwrap();
        let report = build_report(&options(dir.path(), true));
        let inner = report
            .findings
            .iter()
            .filter(|f| f.path.ends_with("node_modules/outro/node_modules"))
            .count();
        assert_eq!(inner, 0, "a target inside another target must not show up");
    }

    #[test]
    fn biggest_dirs_never_repeats_a_parent_and_its_child() {
        let dir = sandbox();
        let report = build_report(&options(dir.path(), true));
        for parent in &report.biggest {
            for child in &report.biggest {
                if parent.path == child.path {
                    continue;
                }
                assert!(
                    !child.path.starts_with(&parent.path),
                    "{} is inside {}",
                    child.path.display(),
                    parent.path.display()
                );
            }
        }
    }

    #[test]
    fn detects_broken_symlinks_and_ignores_working_ones() {
        let dir = sandbox();
        let working = dir.path().join("proj/src/main.rs");
        std::os::unix::fs::symlink(&working, dir.path().join("good-link")).unwrap();
        let report = build_report(&options(dir.path(), true));
        let links: Vec<String> = report
            .broken_links
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(links.contains(&"broken-link".to_string()));
        assert!(!links.contains(&"good-link".to_string()));
    }

    #[test]
    fn respects_the_item_limit_and_flags_truncation() {
        let dir = sandbox();
        let mut opts = options(dir.path(), false);
        opts.limit = 1;
        let report = build_report(&opts);
        assert_eq!(report.findings.len(), 1);
        assert!(report.truncated);
    }

    #[test]
    fn hard_links_are_counted_once_like_du() {
        let dir = sandbox();
        let file = dir.path().join("proj/target/dup.bin");
        fs::write(&file, vec![7u8; 40_000]).unwrap();
        fs::hard_link(&file, dir.path().join("proj/target/dup-2.bin")).unwrap();
        let target = dir.path().join("proj/target");

        let report = build_report(&options(dir.path(), true));
        let mine = report
            .findings
            .iter()
            .find(|finding| finding.path == target)
            .map(|finding| finding.bytes)
            .expect("target in the list");

        let theirs = crate::util::run(
            "du",
            &[
                "-s".into(),
                "-B1".into(),
                "--".into(),
                target.display().to_string(),
            ],
            Duration::from_secs(30),
        )
        .expect("du");
        let reference: u64 = theirs
            .stdout
            .split('\t')
            .next()
            .unwrap_or("0")
            .trim()
            .parse()
            .unwrap();
        assert_eq!(mine, reference, "a hard link counts once, like du");
    }

    #[test]
    fn skipping_dir_measurement_still_finds_every_target() {
        let dir = sandbox();
        let mut opts = options(dir.path(), true);
        opts.measure_dirs = false;

        let report = build_report(&opts);
        assert!(
            report.biggest.is_empty(),
            "no measurement means no biggest dirs"
        );

        let names: Vec<String> = report
            .findings
            .iter()
            .map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"node_modules".to_string()));
        assert!(names.contains(&"target".to_string()));
        assert!(report.reclaimable > 0);
    }

    #[test]
    fn block_usage_matches_du_for_a_known_tree() {
        let dir = sandbox();
        let target = dir.path().join("proj/node_modules");
        let mine = subtree_bytes(&target);
        let theirs = crate::util::run(
            "du",
            &[
                "-s".into(),
                "-B1".into(),
                "--".into(),
                target.display().to_string(),
            ],
            Duration::from_secs(30),
        )
        .expect("du");
        let first = theirs.stdout.split('\t').next().unwrap_or("0");
        let reference: u64 = first.trim().parse().unwrap();
        assert_eq!(mine, reference, "the count must match du");
    }
}
