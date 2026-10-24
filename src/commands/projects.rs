use std::collections::HashSet;
use std::fs;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::Result;
use clap::Args;
use serde_json::json;

use crate::history;
use crate::scan::subtree_bytes;
use crate::targets::is_protected;
use crate::ui;
use crate::util::{expand_home, human_bytes, is_tty, now_ms, parse_size, tildify};

/// Directory names that are artifacts or VCS metadata; discovery never descends
/// into them, so a `target/` cannot contribute a bogus nested project.
const NO_DESCEND: &[&str] = &[
    "target",
    "node_modules",
    ".next",
    "dist",
    "build",
    "__pycache__",
    ".venv",
    "venv",
    ".git",
];

const NON_INTERACTIVE_REFUSAL: &str = "refusing to delete without --yes on a non-interactive stdin";

#[derive(Args, Debug, Clone)]
pub struct ProjectsArgs {
    /// directory whose children are scanned for projects (default: current directory)
    #[arg(value_name = "PATH", default_value = ".")]
    pub path: PathBuf,

    /// how many directory levels below PATH to search
    #[arg(long, value_name = "N", default_value_t = 1)]
    pub depth: usize,

    /// only consider these artifact kinds (comma-separated, e.g. --only target,node_modules)
    #[arg(long, value_name = "KINDS")]
    pub only: Option<String>,

    /// skip these artifact kinds (comma-separated)
    #[arg(long, value_name = "KINDS")]
    pub except: Option<String>,

    /// include unsafe kinds (dist, build, venv), which are otherwise listed but skipped
    #[arg(long)]
    pub all: bool,

    /// only consider projects whose name contains this string
    #[arg(long, value_name = "NAME")]
    pub project: Option<String>,

    /// only consider artifacts older than this many days
    #[arg(long = "older-than", value_name = "DAYS")]
    pub older_than: Option<i64>,

    /// only consider artifacts at least this large (e.g. 100MB)
    #[arg(long = "min-size", value_name = "SIZE")]
    pub min_size: Option<String>,

    /// actually delete (default is a dry run that deletes nothing)
    #[arg(long)]
    pub apply: bool,

    /// skip the confirmation prompt
    #[arg(long)]
    pub yes: bool,

    /// machine-readable output, one document per run
    #[arg(long)]
    pub json: bool,
}

/// A marker file or directory whose presence makes a directory a project.
const MARKERS: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "go.mod",
    "pyproject.toml",
    "setup.py",
    "Gemfile",
    "composer.json",
    ".git",
];

struct KindDef {
    kind: &'static str,
    dirs: &'static [&'static str],
    safe: bool,
}

/// Canonical kind order; also the order used in prompts and tables.
const KINDS: &[KindDef] = &[
    KindDef {
        kind: "target",
        dirs: &["target"],
        safe: true,
    },
    KindDef {
        kind: "node_modules",
        dirs: &["node_modules"],
        safe: true,
    },
    KindDef {
        kind: "next",
        dirs: &[".next"],
        safe: true,
    },
    KindDef {
        kind: "dist",
        dirs: &["dist"],
        safe: false,
    },
    KindDef {
        kind: "build",
        dirs: &["build"],
        safe: false,
    },
    KindDef {
        kind: "pycache",
        dirs: &["__pycache__"],
        safe: true,
    },
    KindDef {
        kind: "venv",
        dirs: &[".venv", "venv"],
        safe: false,
    },
];

const VALID_KINDS: &str = "target, node_modules, next, dist, build, pycache, venv";

struct KindEntry {
    kind: &'static str,
    /// Every on-disk directory backing this kind (venv may be both `.venv` and `venv`).
    dirs: Vec<DirEntry>,
    bytes: u64,
    safe: bool,
}

struct DirEntry {
    path: PathBuf,
    bytes: u64,
}

struct ProjectPlan {
    name: String,
    path: PathBuf,
    /// Artifact kinds selected for removal.
    kinds: Vec<KindEntry>,
    /// Unsafe kinds found but skipped without `--all`.
    excluded: Vec<KindEntry>,
    total: u64,
}

/// Maps user input (`next`, `.next`) to the canonical kind label.
fn canonical_kind(item: &str) -> Option<&'static str> {
    let normalized = item.trim().trim_start_matches('.').to_ascii_lowercase();
    match normalized.as_str() {
        "target" => Some("target"),
        "node_modules" | "node-modules" | "node" => Some("node_modules"),
        "next" => Some("next"),
        "dist" => Some("dist"),
        "build" => Some("build"),
        "pycache" | "__pycache__" => Some("pycache"),
        "venv" => Some("venv"),
        _ => None,
    }
}

fn parse_kind_list(raw: &str) -> anyhow::Result<HashSet<&'static str>> {
    let mut kinds = HashSet::new();
    for item in raw.split(',') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        match canonical_kind(item) {
            Some(kind) => {
                kinds.insert(kind);
            }
            None => anyhow::bail!("unknown artifact kind: {item} (expected one of: {VALID_KINDS})"),
        }
    }
    Ok(kinds)
}

fn resolve_root(raw: &Path) -> PathBuf {
    let text = raw.to_string_lossy();
    let expanded = if text.starts_with('~') {
        expand_home(&text)
    } else {
        raw.to_path_buf()
    };
    if expanded.is_absolute() {
        return expanded;
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(expanded),
        Err(_) => expanded,
    }
}

fn is_project(dir: &Path) -> bool {
    MARKERS.iter().any(|marker| dir.join(marker).exists())
}

/// Paths equal to or below any configured exclude, resolved the same way as `PATH`.
fn is_excluded(path: &Path, excludes: &[PathBuf]) -> bool {
    excludes
        .iter()
        .any(|exclude| path == exclude || path.starts_with(exclude))
}

/// Breadth-first walk up to `depth` levels below `root`. A marked directory is a
/// project and the walk still descends through it, so nested projects are found
/// too; artifact directories, `.git`, protected tool installs, excluded paths,
/// and symlinks are never entered.
fn discover_projects(
    root: &Path,
    depth: usize,
    name_filter: Option<&str>,
    excludes: &[PathBuf],
) -> Vec<PathBuf> {
    let needle = name_filter.map(|name| name.to_ascii_lowercase());
    let mut projects = Vec::new();
    let mut seen = HashSet::new();
    let mut frontier = vec![root.to_path_buf()];

    for _ in 0..depth {
        if frontier.is_empty() {
            break;
        }
        let mut next = Vec::new();
        for dir in &frontier {
            let Ok(entries) = fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                // `file_type` does not follow links, so symlinked directories are skipped.
                if !file_type.is_dir() {
                    continue;
                }
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if NO_DESCEND.iter().any(|skip| *skip == &*name) {
                    continue;
                }
                let path = entry.path();
                if is_protected(&path) || is_excluded(&path, excludes) {
                    continue;
                }
                if is_project(&path) {
                    let matches = match &needle {
                        Some(needle) => name.to_ascii_lowercase().contains(needle),
                        None => true,
                    };
                    if matches && seen.insert(path.clone()) {
                        projects.push(path.clone());
                    }
                }
                next.push(path);
            }
        }
        frontier = next;
    }

    projects.sort();
    projects
}

/// Measures one artifact directory without following symlinks: a symlinked
/// `target/` counts as the link itself and is never traversed, so sizes and
/// later deletions stay inside the project.
fn measure_dir(path: &Path, excludes: &[PathBuf]) -> Option<DirEntry> {
    let meta = fs::symlink_metadata(path).ok()?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return None;
    }
    if is_protected(path) || is_excluded(path, excludes) {
        return None;
    }
    Some(DirEntry {
        path: path.to_path_buf(),
        bytes: subtree_bytes(path),
    })
}

fn measure_project(project: &Path, excludes: &[PathBuf]) -> Vec<KindEntry> {
    let mut found = Vec::new();
    for def in KINDS {
        let mut dirs = Vec::new();
        for name in def.dirs {
            let candidate = project.join(name);
            if let Some(entry) = measure_dir(&candidate, excludes) {
                dirs.push(entry);
            }
        }
        if dirs.is_empty() {
            continue;
        }
        let bytes = dirs.iter().map(|entry| entry.bytes).sum();
        found.push(KindEntry {
            kind: def.kind,
            dirs,
            bytes,
            safe: def.safe,
        });
    }
    found
}

/// Active `--older-than` / `--min-size` filters, parsed once per run so the cutoff
/// cannot drift while a large tree is measured.
struct Filters {
    older_than_days: Option<i64>,
    min_size: Option<u64>,
    cutoff_ms: Option<i64>,
}

impl Filters {
    fn new(args: &ProjectsArgs) -> anyhow::Result<Self> {
        if let Some(days) = args.older_than {
            if days < 0 {
                anyhow::bail!("--older-than must be 0 or more days");
            }
        }
        let min_size = match args.min_size.as_deref() {
            Some(raw) => Some(parse_size(raw).ok_or_else(|| {
                anyhow::anyhow!("invalid --min-size: {raw} (expected e.g. 100MB)")
            })?),
            None => None,
        };
        Ok(Filters {
            older_than_days: args.older_than,
            min_size,
            cutoff_ms: args
                .older_than
                .map(|days| now_ms() - days.saturating_mul(86_400_000)),
        })
    }

    /// A directory with an unreadable mtime is dropped: the safer outcome when the
    /// user asked for artifacts that are old enough.
    fn retains(&self, dir: &DirEntry) -> bool {
        if let Some(min) = self.min_size {
            if dir.bytes < min {
                return false;
            }
        }
        if let Some(cutoff) = self.cutoff_ms {
            let Some(modified) = modified_ms(&dir.path) else {
                return false;
            };
            if modified > cutoff {
                return false;
            }
        }
        true
    }

    fn describe(&self) -> Option<String> {
        let mut labels = Vec::new();
        if let Some(days) = self.older_than_days {
            labels.push(format!("older than {days} days"));
        }
        if let Some(min) = self.min_size {
            labels.push(format!("at least {}", human_bytes(min)));
        }
        if labels.is_empty() {
            None
        } else {
            Some(labels.join(" · "))
        }
    }
}

fn modified_ms(path: &Path) -> Option<i64> {
    let modified = fs::metadata(path).and_then(|meta| meta.modified()).ok()?;
    modified
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|since| since.as_millis() as i64)
}

fn plan_projects(
    args: &ProjectsArgs,
    root: &Path,
    excludes: &[PathBuf],
    filters: &Filters,
) -> anyhow::Result<Vec<ProjectPlan>> {
    let only = args
        .only
        .as_deref()
        .map(parse_kind_list)
        .transpose()?
        .unwrap_or_default();
    let except = args
        .except
        .as_deref()
        .map(parse_kind_list)
        .transpose()?
        .unwrap_or_default();

    if is_excluded(root, excludes) {
        return Ok(Vec::new());
    }

    let mut plans = Vec::new();
    for project in discover_projects(root, args.depth, args.project.as_deref(), excludes) {
        let name = project
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| project.display().to_string());
        let mut kinds = Vec::new();
        let mut excluded = Vec::new();
        for entry in measure_project(&project, excludes) {
            // Kind selection first, then age/size, then the safe/unsafe split.
            if !only.is_empty() && !only.contains(entry.kind) {
                continue;
            }
            if except.contains(entry.kind) {
                continue;
            }
            let dirs: Vec<DirEntry> = entry
                .dirs
                .into_iter()
                .filter(|dir| filters.retains(dir))
                .collect();
            if dirs.is_empty() {
                continue;
            }
            let bytes = dirs.iter().map(|dir| dir.bytes).sum();
            let entry = KindEntry {
                kind: entry.kind,
                dirs,
                bytes,
                safe: entry.safe,
            };
            if !entry.safe && !args.all {
                excluded.push(entry);
                continue;
            }
            kinds.push(entry);
        }
        let total = kinds.iter().map(|entry| entry.bytes).sum();
        plans.push(ProjectPlan {
            name,
            path: project,
            kinds,
            excluded,
            total,
        });
    }
    plans.sort_by(|a, b| b.total.cmp(&a.total).then_with(|| a.name.cmp(&b.name)));
    Ok(plans)
}

fn distinct_kinds(plans: &[ProjectPlan]) -> Vec<&'static str> {
    let mut kinds: Vec<&'static str> = plans
        .iter()
        .flat_map(|plan| plan.kinds.iter().map(|entry| entry.kind))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    kinds.sort_by_key(|kind| KINDS.iter().position(|def| def.kind == *kind).unwrap_or(99));
    kinds
}

fn confirm(question: &str) -> bool {
    print!("{question}");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// One prompt for the whole run after the plan has been shown. `--yes` skips it;
/// a non-interactive stdin is refused instead of auto-confirmed, so a piped run
/// can never delete without an explicit `--yes`.
fn confirmed(plans: &[ProjectPlan], total: u64, yes: bool, tty: bool) -> bool {
    if yes {
        return true;
    }
    if !tty {
        eprintln!("{}", ui::bad(NON_INTERACTIVE_REFUSAL));
        return false;
    }
    let kinds = distinct_kinds(plans).join(", ");
    confirm(&format!(
        "Remove {kinds} from {} projects ({})? [y/N] ",
        plans.len(),
        human_bytes(total)
    ))
}

fn emit_json(
    root: &Path,
    args: &ProjectsArgs,
    plans: &[ProjectPlan],
    removed: Option<(usize, u64, &[String])>,
) -> Result<()> {
    let projects = plans
        .iter()
        .map(|plan| {
            json!({
                "name": plan.name,
                "path": plan.path.display().to_string(),
                "kinds": plan.kinds.iter().map(|entry| json!({
                    "kind": entry.kind,
                    "bytes": entry.bytes,
                    "path": entry.dirs.iter().map(|dir| dir.path.display().to_string()).collect::<Vec<_>>(),
                })).collect::<Vec<_>>(),
                "total": plan.total,
            })
        })
        .collect::<Vec<_>>();
    let mut document = json!({
        "path": root.display().to_string(),
        "depth": args.depth,
        "olderThan": args.older_than,
        "minSize": args.min_size,
        "apply": args.apply,
        "projects": projects,
    });
    let (removed_count, freed, errors) = removed.unwrap_or((0, 0, &[]));
    document["removed"] = json!(removed_count);
    document["freed"] = json!(freed);
    document["errors"] = json!(errors);
    println!("{}", serde_json::to_string_pretty(&document)?);
    Ok(())
}

fn print_plan(
    root: &Path,
    args: &ProjectsArgs,
    plans: &[ProjectPlan],
    total: u64,
    filters: &Filters,
) {
    ui::section("tend", Some("projects"));
    ui::kv("root", &tildify(root));
    ui::kv("depth", &args.depth.to_string());
    if let Some(active) = filters.describe() {
        ui::kv("filters", &active);
    }
    ui::kv(
        "projects",
        &format!(
            "{} with artifacts{}",
            plans.len(),
            if args.all { " (including unsafe)" } else { "" }
        ),
    );
    ui::kv("reclaimable", &ui::bold(&human_bytes(total)));
    ui::blank();

    let rows: Vec<Vec<String>> = plans
        .iter()
        .map(|plan| {
            let kinds = plan
                .kinds
                .iter()
                .map(|entry| format!("{} ({})", entry.kind, human_bytes(entry.bytes)))
                .collect::<Vec<_>>()
                .join(", ");
            vec![plan.name.clone(), kinds, human_bytes(plan.total)]
        })
        .collect();
    if !rows.is_empty() {
        ui::table(&["project", "artifacts", "size"], &rows);
        ui::blank();
    }

    let skipped: Vec<&ProjectPlan> = plans
        .iter()
        .filter(|plan| !plan.excluded.is_empty())
        .collect();
    if !skipped.is_empty() {
        let names: Vec<String> = skipped
            .iter()
            .flat_map(|plan| {
                plan.excluded
                    .iter()
                    .map(|entry| format!("{} in {}", entry.kind, plan.name))
            })
            .collect();
        ui::say_note(&format!(
            "skipped unsafe (rerun with --all to include): {}",
            names.join(", ")
        ));
    }
}

/// Deletes every selected artifact directory. Each path is re-validated right
/// before removal: it must still be a real directory (not a symlink), still
/// sit inside its project, and still fall outside the protected tool-install
/// prefixes.
fn delete_all(plans: &[ProjectPlan], quiet: bool) -> (usize, u64, Vec<String>) {
    let mut removed = 0usize;
    let mut freed = 0u64;
    let mut errors: Vec<String> = Vec::new();

    for plan in plans {
        for entry in &plan.kinds {
            for dir in &entry.dirs {
                let meta = match fs::symlink_metadata(&dir.path) {
                    Ok(meta) => meta,
                    Err(error) => {
                        errors.push(format!("{}: {error}", dir.path.display()));
                        continue;
                    }
                };
                if meta.file_type().is_symlink() {
                    errors.push(format!("{}: skipped symlink", dir.path.display()));
                    continue;
                }
                if !meta.is_dir() {
                    continue;
                }
                if !dir.path.starts_with(&plan.path) {
                    errors.push(format!("{}: outside project, skipped", dir.path.display()));
                    continue;
                }
                if is_protected(&dir.path) {
                    continue;
                }
                match fs::remove_dir_all(&dir.path) {
                    Ok(()) => {
                        removed += 1;
                        freed += dir.bytes;
                        if !quiet {
                            ui::bullet(
                                &format!(
                                    "{} {}",
                                    ui::grey(&tildify(&dir.path)),
                                    ui::dim(&human_bytes(dir.bytes))
                                ),
                                2,
                            );
                        }
                    }
                    Err(error) => errors.push(format!("{}: {error}", dir.path.display())),
                }
            }
        }
    }

    (removed, freed, errors)
}

pub fn run(args: ProjectsArgs) -> Result<i32> {
    let root = resolve_root(&args.path);
    let config = crate::config::load().config;
    let excludes: Vec<PathBuf> = config
        .exclude
        .iter()
        .map(|raw| resolve_root(Path::new(raw)))
        .collect();
    let filters = Filters::new(&args)?;

    if !root.is_dir() {
        let error = format!("not a directory: {}", root.display());
        if args.json {
            emit_json(&root, &args, &[], Some((0, 0, &[error])))?;
        } else {
            ui::say_fail(&format!("not a directory: {}", tildify(&root)));
        }
        return Ok(1);
    }

    let plans = plan_projects(&args, &root, &excludes, &filters)?;
    let actionable: Vec<ProjectPlan> = plans
        .into_iter()
        .filter(|plan| !plan.kinds.is_empty())
        .collect();
    let total: u64 = actionable.iter().map(|plan| plan.total).sum();
    let interactive = is_tty() && std::io::stdin().is_terminal();

    if args.apply && !args.yes && !interactive {
        // Piped stdin cannot answer the prompt: refuse before anything is removed, even
        // when the plan is empty, so the run is never mistaken for a successful apply.
        if args.json {
            emit_json(&root, &args, &actionable, Some((0, 0, &[])))?;
        } else {
            print_plan(&root, &args, &actionable, total, &filters);
        }
        eprintln!("{}", ui::bad(NON_INTERACTIVE_REFUSAL));
        return Ok(1);
    }

    if args.json {
        if !args.apply {
            emit_json(&root, &args, &actionable, None)?;
            return Ok(0);
        }
        if actionable.is_empty() {
            emit_json(&root, &args, &actionable, Some((0, 0, &[])))?;
            return Ok(0);
        }
        if !confirmed(&actionable, total, args.yes, interactive) {
            emit_json(&root, &args, &actionable, Some((0, 0, &[])))?;
            eprintln!("cancelled, nothing was removed");
            return Ok(1);
        }
        let (removed, freed, errors) = delete_all(&actionable, true);
        history::record(
            "projects",
            std::slice::from_ref(&root),
            removed,
            freed,
            errors.len(),
        );
        let code = if errors.is_empty() { 0 } else { 1 };
        emit_json(&root, &args, &actionable, Some((removed, freed, &errors)))?;
        return Ok(code);
    }

    print_plan(&root, &args, &actionable, total, &filters);
    if actionable.is_empty() {
        ui::say_note("no project artifacts found");
        return Ok(0);
    }

    if !args.apply {
        ui::say_info(&format!(
            "dry run, nothing was removed: {}",
            ui::bold("rerun with --apply to delete")
        ));
        return Ok(0);
    }

    ui::say_warn(&format!(
        "destructive mode, {} will be removed",
        ui::bold(&human_bytes(total))
    ));
    if !confirmed(&actionable, total, args.yes, interactive) {
        ui::say_fail("cancelled, nothing was removed");
        return Ok(1);
    }

    let (removed, freed, errors) = delete_all(&actionable, false);
    history::record(
        "projects",
        std::slice::from_ref(&root),
        removed,
        freed,
        errors.len(),
    );
    ui::blank();
    ui::say_ok(&format!(
        "{removed} directories removed, {} freed",
        ui::bold(&human_bytes(freed))
    ));
    for error in errors.iter().take(5) {
        ui::say_fail(error);
    }
    Ok(if errors.is_empty() { 0 } else { 1 })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_args(path: &Path) -> ProjectsArgs {
        ProjectsArgs {
            path: path.to_path_buf(),
            depth: 1,
            only: None,
            except: None,
            all: false,
            project: None,
            older_than: None,
            min_size: None,
            apply: false,
            yes: true,
            json: false,
        }
    }

    fn names(paths: &[PathBuf]) -> Vec<String> {
        paths
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    fn actionable(plans: &[ProjectPlan]) -> Vec<&ProjectPlan> {
        plans.iter().filter(|plan| !plan.kinds.is_empty()).collect()
    }

    /// Plans with freshly parsed filters, mirroring what `run` does.
    fn plan(args: &ProjectsArgs, root: &Path, excludes: &[PathBuf]) -> Vec<ProjectPlan> {
        let filters = Filters::new(args).unwrap();
        plan_projects(args, root, excludes, &filters).unwrap()
    }

    fn write_file(path: &Path, contents: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    /// A root with a Rust project, a Node project, and a plain directory.
    fn fixture() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        let rust = root.join("rust-app");
        write_file(
            &rust.join("Cargo.toml"),
            b"[package]\nname = \"rust-app\"\n",
        );
        write_file(
            &rust.join("target").join("debug").join("blob"),
            &[0u8; 4096],
        );

        let web = root.join("web");
        write_file(&web.join("package.json"), b"{\"name\":\"web\"}\n");
        write_file(
            &web.join("node_modules").join("dep").join("index.js"),
            b"module.exports = 1;\n",
        );

        let plain = root.join("notes");
        write_file(&plain.join("README.md"), b"just notes\n");

        tmp
    }

    #[test]
    fn discovery_finds_cargo_and_node_projects() {
        let tmp = fixture();
        let found = discover_projects(tmp.path(), 1, None, &[]);
        assert_eq!(
            names(&found),
            vec!["rust-app".to_string(), "web".to_string()]
        );
    }

    #[test]
    fn only_filter_selects_a_subset() {
        let tmp = fixture();
        let args = ProjectsArgs {
            only: Some("target".to_string()),
            ..base_args(tmp.path())
        };
        let plans = plan(&args, tmp.path(), &[]);
        let actionable: Vec<&ProjectPlan> =
            plans.iter().filter(|plan| !plan.kinds.is_empty()).collect();
        assert_eq!(actionable.len(), 1);
        assert_eq!(actionable[0].name, "rust-app");
        assert_eq!(actionable[0].kinds.len(), 1);
        assert_eq!(actionable[0].kinds[0].kind, "target");
    }

    #[test]
    fn project_filter_matches_by_substring() {
        let tmp = fixture();
        let args = ProjectsArgs {
            project: Some("web".to_string()),
            ..base_args(tmp.path())
        };
        let plans = plan(&args, tmp.path(), &[]);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].name, "web");
    }

    #[test]
    fn dry_run_deletes_nothing() {
        let tmp = fixture();
        let args = base_args(tmp.path());
        let code = run(args).unwrap();
        assert_eq!(code, 0);
        assert!(tmp.path().join("rust-app").join("target").is_dir());
        assert!(tmp.path().join("web").join("node_modules").is_dir());
    }

    #[test]
    fn apply_deletes_artifacts_but_keeps_sources() {
        let tmp = fixture();
        let args = ProjectsArgs {
            apply: true,
            ..base_args(tmp.path())
        };
        let code = run(args).unwrap();
        assert_eq!(code, 0);
        assert!(!tmp.path().join("rust-app").join("target").exists());
        assert!(!tmp.path().join("web").join("node_modules").exists());
        assert!(tmp.path().join("rust-app").join("Cargo.toml").exists());
        assert!(tmp.path().join("web").join("package.json").exists());
    }

    #[test]
    fn unsafe_kinds_need_all() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("py-app");
        write_file(
            &proj.join("pyproject.toml"),
            b"[project]\nname = \"py-app\"\n",
        );
        write_file(
            &proj.join(".venv").join("bin").join("activate"),
            b"# venv\n",
        );

        let root = tmp.path();
        let without_all = plan(&base_args(root), root, &[]);
        assert!(without_all.iter().all(|plan| plan.kinds.is_empty()));
        assert!(
            without_all
                .iter()
                .any(|plan| plan.excluded.iter().any(|entry| entry.kind == "venv"))
        );

        let args = ProjectsArgs {
            all: true,
            ..base_args(root)
        };
        let with_all = plan(&args, root, &[]);
        let actionable: Vec<&ProjectPlan> = with_all
            .iter()
            .filter(|plan| !plan.kinds.is_empty())
            .collect();
        assert_eq!(actionable.len(), 1);
        assert!(actionable[0].kinds.iter().any(|entry| entry.kind == "venv"));
    }

    #[test]
    fn nested_project_needs_matching_depth() {
        let tmp = tempfile::tempdir().unwrap();
        let inner = tmp.path().join("group").join("inner");
        write_file(&inner.join("Cargo.toml"), b"[package]\nname = \"inner\"\n");
        write_file(&inner.join("target").join("blob"), &[0u8; 2048]);

        assert!(discover_projects(tmp.path(), 1, None, &[]).is_empty());

        let deep = discover_projects(tmp.path(), 2, None, &[]);
        assert_eq!(names(&deep), vec!["inner".to_string()]);

        let shallow_args = base_args(tmp.path());
        assert!(actionable(&plan(&shallow_args, tmp.path(), &[])).is_empty());

        let deep_args = ProjectsArgs {
            depth: 2,
            ..base_args(tmp.path())
        };
        let plans = plan(&deep_args, tmp.path(), &[]);
        assert_eq!(actionable(&plans).len(), 1);
        assert_eq!(actionable(&plans)[0].name, "inner");
    }

    #[test]
    fn artifact_dirs_are_never_descended() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().join("app");
        write_file(&app.join("Cargo.toml"), b"[package]\nname = \"app\"\n");
        write_file(
            &app.join("target").join("nested").join("Cargo.toml"),
            b"[package]\nname = \"nested\"\n",
        );

        let found = discover_projects(tmp.path(), 3, None, &[]);
        assert_eq!(names(&found), vec!["app".to_string()]);
    }

    #[test]
    fn excluded_projects_and_artifacts_are_skipped() {
        let tmp = fixture();
        let root = tmp.path();

        let excludes = vec![root.join("rust-app")];
        let plans = plan(&base_args(root), root, &excludes);
        assert!(plans.iter().all(|plan| plan.name != "rust-app"));
        assert!(plans.iter().any(|plan| plan.name == "web"));

        let excludes = vec![root.join("web").join("node_modules")];
        let plans = plan(&base_args(root), root, &excludes);
        let web = plans.iter().find(|plan| plan.name == "web").unwrap();
        assert!(web.kinds.is_empty());
    }

    #[test]
    fn excluded_root_yields_no_plan() {
        let tmp = fixture();
        let excludes = vec![tmp.path().to_path_buf()];
        let plans = plan(&base_args(tmp.path()), tmp.path(), &excludes);
        assert!(plans.is_empty());
    }

    #[test]
    fn min_size_skips_small_artifacts() {
        let tmp = fixture();
        let args = ProjectsArgs {
            min_size: Some("1MB".to_string()),
            ..base_args(tmp.path())
        };
        let plans = plan(&args, tmp.path(), &[]);
        assert!(plans.iter().all(|plan| plan.kinds.is_empty()));
    }

    #[test]
    fn older_than_skips_fresh_artifacts() {
        let tmp = fixture();
        let args = ProjectsArgs {
            older_than: Some(7),
            ..base_args(tmp.path())
        };
        let plans = plan(&args, tmp.path(), &[]);
        assert!(plans.iter().all(|plan| plan.kinds.is_empty()));
    }

    #[test]
    fn invalid_filters_are_rejected() {
        let tmp = fixture();
        let bad_size = ProjectsArgs {
            min_size: Some("big".to_string()),
            ..base_args(tmp.path())
        };
        assert!(Filters::new(&bad_size).is_err());

        let negative = ProjectsArgs {
            older_than: Some(-1),
            ..base_args(tmp.path())
        };
        assert!(Filters::new(&negative).is_err());
    }

    #[test]
    fn non_interactive_apply_requires_yes() {
        assert!(!confirmed(&[], 0, false, false));
        assert!(confirmed(&[], 0, true, false));
    }
}
