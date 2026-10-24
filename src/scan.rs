use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::targets::{self, Target};
use crate::util::{Disk, disk, is_tty, now_ms, run};

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
    pub total_findings: usize,
    pub reclaimable: u64,
    pub reclaimable_safe: u64,
    pub repos: Vec<Repo>,
    pub stale: Vec<Repo>,
    pub broken_links: Vec<PathBuf>,
    pub links_total: usize,
    pub links_capped: bool,
    pub truncated: bool,
    pub protected_paths: Vec<PathBuf>,
    pub stale_days: i64,
}

pub struct ScanOptions {
    pub roots: Vec<PathBuf>,
    /// Path prefixes that are never entered: nothing below them is measured or
    /// reported.
    pub exclude: Vec<PathBuf>,
    pub depth: usize,
    pub stale_days: i64,
    pub safe_only: bool,
    pub limit: usize,
    pub repos: bool,
    pub links: bool,
    /// Measuring the whole tree is only needed for the "biggest directories" view.
    /// Without it the walk behaves like find: it stats directories, not every file.
    pub measure_dirs: bool,
    /// Print progress to stderr while walking.
    pub progress: bool,
}

const LINK_LIMIT: usize = 40;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
const PROGRESS_STEP: u64 = 2000;

/// Throttled stderr progress, shared by every walker thread.
struct Progress {
    enabled: bool,
    dirs: AtomicU64,
    printed: AtomicBool,
    last: Mutex<Instant>,
}

impl Progress {
    fn new(options: &ScanOptions) -> Self {
        Progress {
            enabled: options.progress && is_tty(),
            dirs: AtomicU64::new(0),
            printed: AtomicBool::new(false),
            last: Mutex::new(Instant::now()),
        }
    }

    fn note_dir(&self) {
        if !self.enabled {
            return;
        }
        let count = self.dirs.fetch_add(1, Ordering::Relaxed) + 1;
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        if last.elapsed() >= PROGRESS_INTERVAL || count % PROGRESS_STEP == 0 {
            *last = Instant::now();
            eprint!("\r  scanned {count} directories ...");
            self.printed.store(true, Ordering::Relaxed);
        }
    }

    fn clear(&self) {
        if self.enabled && self.printed.load(Ordering::Relaxed) {
            eprint!("\r\x1b[K");
        }
    }
}

/// Everything a walk collects. Each thread keeps its own accumulator; the results
/// are merged once at the end.
#[derive(Default)]
struct Accumulator {
    dirs: Vec<DirSize>,
    targets: Vec<(PathBuf, u64)>,
    protected: Vec<PathBuf>,
    links: Vec<PathBuf>,
    links_total: usize,
    repos: Vec<PathBuf>,
}

impl Accumulator {
    fn merge(&mut self, mut other: Accumulator) {
        self.dirs.append(&mut other.dirs);
        self.targets.append(&mut other.targets);
        self.protected.append(&mut other.protected);
        self.links.append(&mut other.links);
        self.links_total += other.links_total;
        self.repos.append(&mut other.repos);
    }
}

fn target_matches(name: &str, safe_only: bool) -> bool {
    match targets::find(name) {
        Some(target) => !safe_only || target.safe,
        None => false,
    }
}

/// Paths at or below an excluded prefix are never entered: they contribute no
/// bytes and can never surface as findings, repos, or links.
fn is_excluded(path: &Path, excludes: &[PathBuf]) -> bool {
    excludes
        .iter()
        .any(|prefix| path == prefix || path.starts_with(prefix))
}

/// A depth-1 directory handed to a worker thread.
struct Job {
    path: PathBuf,
    depth: usize,
    root_dev: u64,
    pruned: bool,
}

struct Walker<'a> {
    max_depth: usize,
    safe_only: bool,
    measure_dirs: bool,
    want_repos: bool,
    want_links: bool,
    exclude: Vec<PathBuf>,
    root_dev: u64,
    acc: Accumulator,
    /// Hard links share their blocks; du counts each inode once and so does this.
    /// Shared across walkers so parallel workers agree with `du`.
    seen: Arc<Mutex<HashSet<(u64, u64)>>>,
    progress: &'a Progress,
}

impl<'a> Walker<'a> {
    fn new(
        options: &ScanOptions,
        root_dev: u64,
        seen: Arc<Mutex<HashSet<(u64, u64)>>>,
        progress: &'a Progress,
    ) -> Self {
        Walker {
            max_depth: options.depth,
            safe_only: options.safe_only,
            measure_dirs: options.measure_dirs,
            want_repos: options.repos,
            want_links: options.links,
            exclude: options.exclude.clone(),
            root_dev,
            acc: Accumulator::default(),
            seen,
            progress,
        }
    }

    fn matches_target(&self, name: &str) -> bool {
        target_matches(name, self.safe_only)
    }

    /// Excluded prefixes are never entered, so they contribute no bytes and can
    /// never surface as findings, repos, or links.
    fn excluded(&self, path: &Path) -> bool {
        is_excluded(path, &self.exclude)
    }

    fn record_link(&mut self, path: PathBuf, depth: usize) {
        if !self.want_links || depth > self.max_depth || fs::metadata(&path).is_ok() {
            return;
        }
        self.acc.links_total += 1;
        if self.acc.links.len() < LINK_LIMIT {
            self.acc.links.push(path);
        }
    }

    /// Walks the root's own entries. Target children are measured inline; ordinary
    /// child directories are queued so workers can walk them in parallel.
    fn walk_root(&mut self, root: &Path, jobs: &mut Vec<Job>) {
        self.progress.note_dir();
        let measuring = self.measure_dirs;
        let Ok(entries) = fs::read_dir(root) else {
            return;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if self.excluded(&path) {
                continue;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let child_depth = 1;

            if file_type.is_symlink() {
                if measuring {
                    let Ok(meta) = entry.metadata() else {
                        continue;
                    };
                    if meta.dev() != self.root_dev {
                        continue;
                    }
                }
                self.record_link(path, child_depth);
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
                let is_target = child_depth <= self.max_depth && self.matches_target(&name);

                if self.want_repos && name == ".git" && child_depth <= self.max_depth {
                    if let Some(parent) = path.parent() {
                        self.acc.repos.push(parent.to_path_buf());
                    }
                }

                let child_pruned = is_target || name == ".cache";
                if is_target {
                    let measured = self.walk(&path, child_depth, true, child_pruned);
                    if targets::is_protected(&path) {
                        self.acc.protected.push(path.clone());
                    } else {
                        self.acc.targets.push((path.clone(), measured));
                    }
                    if self.measure_dirs && child_depth <= self.max_depth {
                        self.acc.dirs.push(DirSize {
                            path,
                            bytes: measured,
                        });
                    }
                } else if measuring || child_depth <= self.max_depth {
                    jobs.push(Job {
                        path,
                        depth: child_depth,
                        root_dev: self.root_dev,
                        pruned: child_pruned,
                    });
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
                if meta.nlink() > 1 {
                    let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
                    if !seen.insert((meta.dev(), meta.ino())) {
                        continue;
                    }
                }
            }
        }
    }

    /// Returns the total block usage of `dir` and records the findings found on the
    /// way. Target directories are pruned from further matching but still measured.
    /// `pruned` marks a subtree where a `.git` is build output, not a project, or
    /// where nothing may be reported as a target (`.cache`).
    fn walk(&mut self, dir: &Path, depth: usize, inside_target: bool, pruned: bool) -> u64 {
        if self.excluded(dir) {
            return 0;
        }
        self.progress.note_dir();
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
            if self.excluded(&path) {
                continue;
            }
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
                self.record_link(path, child_depth);
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
                let is_target = !inside_target
                    && !pruned
                    && child_depth <= self.max_depth
                    && self.matches_target(&name);

                if self.want_repos && name == ".git" && child_depth <= self.max_depth && !pruned {
                    if let Some(parent) = path.parent() {
                        self.acc.repos.push(parent.to_path_buf());
                    }
                }

                let child_pruned = pruned || is_target || name == ".cache";
                let bytes = if is_target {
                    let measured = self.walk(&path, child_depth, true, child_pruned);
                    if targets::is_protected(&path) {
                        self.acc.protected.push(path.clone());
                    } else {
                        self.acc.targets.push((path.clone(), measured));
                    }
                    measured
                } else if measuring || child_depth <= self.max_depth {
                    self.walk(&path, child_depth, inside_target, child_pruned)
                } else {
                    0
                };

                total += bytes;
                if self.measure_dirs && child_depth <= self.max_depth {
                    self.acc.dirs.push(DirSize { path, bytes });
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
                if meta.nlink() > 1 {
                    let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
                    if !seen.insert((meta.dev(), meta.ino())) {
                        continue;
                    }
                }
                total += usage(&meta);
            }
        }

        total
    }
}

/// Walks the queued subtrees with a bounded pool of scoped threads.
fn walk_jobs(
    options: &ScanOptions,
    jobs: &[Job],
    seen: &Arc<Mutex<HashSet<(u64, u64)>>>,
    progress: &Progress,
) -> Vec<Accumulator> {
    let workers = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1)
        .clamp(1, 8)
        .min(jobs.len());
    let cursor = AtomicUsize::new(0);

    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(workers);
        for _ in 0..workers {
            let cursor = &cursor;
            let seen = Arc::clone(seen);
            handles.push(scope.spawn(move || {
                let mut walker = Walker::new(options, 0, seen, progress);
                loop {
                    let index = cursor.fetch_add(1, Ordering::Relaxed);
                    let Some(job) = jobs.get(index) else {
                        break;
                    };
                    if walker.excluded(&job.path) {
                        continue;
                    }
                    walker.root_dev = job.root_dev;
                    let bytes = walker.walk(&job.path, job.depth, false, job.pruned);
                    if walker.measure_dirs && job.depth <= walker.max_depth {
                        walker.acc.dirs.push(DirSize {
                            path: job.path.clone(),
                            bytes,
                        });
                    }
                }
                walker.acc
            }));
        }
        handles
            .into_iter()
            .map(|handle| handle.join().expect("walk worker panicked"))
            .collect::<Vec<_>>()
    })
}

/// Sizes for the top-level directories, dropping any entry already explained by an
/// ancestor that is also in the list.
fn biggest_dirs(mut dirs: Vec<DirSize>, top: usize) -> Vec<DirSize> {
    dirs.sort_by(|a, b| {
        b.bytes
            .cmp(&a.bytes)
            .then_with(|| {
                a.path
                    .components()
                    .count()
                    .cmp(&b.path.components().count())
            })
            .then_with(|| a.path.cmp(&b.path))
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
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    let dev = meta.dev();
    let mut seen = HashSet::new();
    subtree_with_seen(path, dev, &mut seen)
}

fn subtree_with_seen(path: &Path, dev: u64, seen: &mut HashSet<(u64, u64)>) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    if meta.dev() != dev {
        return 0;
    }
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
            if child_meta.dev() != dev {
                continue;
            }
            if child_meta.nlink() > 1
                && !child_meta.is_dir()
                && !seen.insert((child_meta.dev(), child_meta.ino()))
            {
                continue;
            }
        }
        total += subtree_with_seen(&child, dev, seen);
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
    repos.sort_by(|a, b| {
        a.last_commit_ms
            .cmp(&b.last_commit_ms)
            .then_with(|| a.path.cmp(&b.path))
    });
    repos
}

pub fn build_report(options: &ScanOptions) -> Report {
    let seen = Arc::new(Mutex::new(HashSet::new()));
    let progress = Progress::new(options);
    let mut accumulator = Accumulator::default();

    for root in &options.roots {
        if is_excluded(root, &options.exclude) {
            continue;
        }
        let root_dev = fs::symlink_metadata(root)
            .map(|meta| meta.dev())
            .unwrap_or(0);
        let mut jobs = Vec::new();
        {
            let mut walker = Walker::new(options, root_dev, Arc::clone(&seen), &progress);
            walker.walk_root(root, &mut jobs);
            accumulator.merge(walker.acc);
        }
        if !jobs.is_empty() {
            for result in walk_jobs(options, &jobs, &seen, &progress) {
                accumulator.merge(result);
            }
        }
    }

    progress.clear();

    let mut findings: Vec<Finding> = accumulator
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
    findings.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.path.cmp(&b.path)));
    let total_findings = findings.len();
    let reclaimable: u64 = findings.iter().map(|f| f.bytes).sum();
    let reclaimable_safe: u64 = findings
        .iter()
        .filter(|f| f.target.safe)
        .map(|f| f.bytes)
        .sum();
    let truncated = total_findings > options.limit;
    findings.truncate(options.limit);

    let repos = if options.repos {
        let mut unique = accumulator.repos;
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

    let mut protected_paths = accumulator.protected;
    protected_paths.sort();
    protected_paths.dedup();

    let links_total = accumulator.links_total;
    let mut broken_links = accumulator.links;
    broken_links.sort();
    broken_links.dedup();
    broken_links.truncate(LINK_LIMIT);
    let links_capped = links_total > LINK_LIMIT;

    let biggest = biggest_dirs(accumulator.dirs, 8);
    let disk = options.roots.first().and_then(|root| disk(root));

    Report {
        roots: options.roots.clone(),
        disk,
        biggest,
        findings,
        total_findings,
        reclaimable,
        reclaimable_safe,
        repos,
        stale,
        broken_links,
        links_total,
        links_capped,
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
            "totalFindings": self.total_findings,
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
            "linksTotal": self.links_total,
            "linksCapped": self.links_capped,
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
            exclude: Vec::new(),
            depth: 6,
            stale_days: 180,
            safe_only,
            limit: 400,
            repos: false,
            links: true,
            measure_dirs: true,
            progress: false,
        }
    }

    #[test]
    fn excluded_prefix_contributes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("keep/node_modules/pkg")).unwrap();
        fs::write(
            root.join("keep/node_modules/pkg/index.js"),
            "k".repeat(8192),
        )
        .unwrap();
        fs::create_dir_all(root.join("skip/node_modules/pkg")).unwrap();
        fs::write(
            root.join("skip/node_modules/pkg/index.js"),
            "s".repeat(8192),
        )
        .unwrap();

        let mut opts = options(root, true);
        opts.exclude = vec![root.join("skip")];
        let report = build_report(&opts);

        let found: Vec<PathBuf> = report.findings.iter().map(|f| f.path.clone()).collect();
        assert_eq!(found, vec![root.join("keep/node_modules")]);
        let keep = root.join("keep/node_modules");
        assert_eq!(report.reclaimable, subtree_bytes(&keep));
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
        let full = build_report(&opts);
        assert!(
            full.total_findings > 1,
            "the sandbox needs more than one finding"
        );

        opts.limit = 1;
        let report = build_report(&opts);
        assert_eq!(report.findings.len(), 1);
        assert!(report.truncated);
        assert_eq!(report.total_findings, full.total_findings);
        assert_eq!(report.reclaimable, full.reclaimable);
        assert_eq!(report.reclaimable_safe, full.reclaimable_safe);
    }

    #[test]
    fn targets_inside_a_cache_dir_are_never_findings() {
        let dir = sandbox();
        let cached = dir.path().join(".cache/tool/node_modules");
        fs::create_dir_all(&cached).unwrap();
        fs::write(cached.join("cached.js"), "z".repeat(4096)).unwrap();
        let cached_target = dir.path().join(".cache/tool/target/debug");
        fs::create_dir_all(&cached_target).unwrap();
        fs::write(cached_target.join("bin"), "bin").unwrap();

        for safe_only in [true, false] {
            let report = build_report(&options(dir.path(), safe_only));
            let cache = dir.path().join(".cache");
            assert!(
                report
                    .findings
                    .iter()
                    .all(|finding| !finding.path.starts_with(&cache)),
                "a target inside .cache must never be reported"
            );
            assert!(
                report
                    .findings
                    .iter()
                    .any(|finding| finding.path == dir.path().join("proj/node_modules")),
                "project caches are still reported"
            );
            assert!(
                report.biggest.iter().any(|entry| entry.path == cache),
                ".cache is still measured for the biggest-directories view"
            );
        }
    }

    #[test]
    fn caps_broken_link_paths_but_counts_all() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for index in 0..45 {
            std::os::unix::fs::symlink(
                root.join(format!("missing-{index}")),
                root.join(format!("link-{index}")),
            )
            .unwrap();
        }

        let report = build_report(&options(root, true));
        assert_eq!(report.links_total, 45);
        assert_eq!(report.broken_links.len(), LINK_LIMIT);
        assert!(report.links_capped);
    }

    #[test]
    fn parallel_walk_is_deterministic() {
        let dir = sandbox();
        for index in 0..6 {
            let base = dir.path().join(format!("extra{index}"));
            fs::create_dir_all(base.join("node_modules/pkg")).unwrap();
            fs::write(
                base.join("node_modules/pkg/file.js"),
                vec![b'x'; 1024 * (index + 1)],
            )
            .unwrap();
            fs::create_dir_all(base.join("target/debug")).unwrap();
            fs::write(
                base.join("target/debug/bin"),
                vec![b'y'; 2048 * (index + 1)],
            )
            .unwrap();
        }

        let opts = options(dir.path(), false);
        let first = build_report(&opts);
        let second = build_report(&opts);

        let snapshot = |report: &Report| -> Vec<(PathBuf, u64)> {
            report
                .findings
                .iter()
                .map(|finding| (finding.path.clone(), finding.bytes))
                .collect()
        };
        assert_eq!(snapshot(&first), snapshot(&second));
        assert_eq!(first.total_findings, second.total_findings);
        assert_eq!(first.reclaimable, second.reclaimable);
        assert_eq!(first.reclaimable_safe, second.reclaimable_safe);
        assert_eq!(first.broken_links, second.broken_links);
        assert_eq!(first.links_total, second.links_total);
        assert_eq!(first.protected_paths, second.protected_paths);
        let biggest = |report: &Report| -> Vec<(PathBuf, u64)> {
            report
                .biggest
                .iter()
                .map(|entry| (entry.path.clone(), entry.bytes))
                .collect()
        };
        assert_eq!(biggest(&first), biggest(&second));
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
