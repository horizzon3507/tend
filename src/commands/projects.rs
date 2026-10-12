use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Args;
use serde_json::json;

use crate::scan::subtree_bytes;
use crate::targets::is_protected;
use crate::ui;
use crate::util::{expand_home, human_bytes, is_tty, tildify};

#[derive(Args, Debug, Clone)]
pub struct ProjectsArgs {
    /// directory whose immediate children are scanned for projects (default: current directory)
    #[arg(value_name = "PATH", default_value = ".")]
    pub path: PathBuf,

    /// only consider these artifact kinds (comma-separated, e.g. --only target,node_modules)
    #[arg(long, value_name = "KINDS")]
    pub only: Option<String>,

    /// skip these artifact kinds (comma-separated)
    #[arg(long, value_name = "KINDS")]
    pub except: Option<String>,

    /// include unsafe kinds (dist, build, venv), which are otherwise listed but skipped
    #[arg(long)]
    pub all: bool,

    /// only consider the project whose name contains this string
    #[arg(long, value_name = "NAME")]
    pub project: Option<String>,

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

/// Immediate child directories of `root` that look like projects. Symlinked
/// children are skipped so a link can never pull a scan outside the root.
fn discover_projects(root: &Path, name_filter: Option<&str>) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let needle = name_filter.map(|name| name.to_ascii_lowercase());
    let mut projects = Vec::new();
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let path = entry.path();
        if is_protected(&path) {
            continue;
        }
        if let Some(needle) = &needle {
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if !name.contains(needle) {
                continue;
            }
        }
        if is_project(&path) {
            projects.push(path);
        }
    }
    projects.sort();
    projects
}

/// Measures one artifact directory without following symlinks: a symlinked
/// `target/` counts as the link itself and is never traversed, so sizes and
/// later deletions stay inside the project.
fn measure_dir(path: &Path) -> Option<DirEntry> {
    let meta = fs::symlink_metadata(path).ok()?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return None;
    }
    if is_protected(path) {
        return None;
    }
    Some(DirEntry {
        path: path.to_path_buf(),
        bytes: subtree_bytes(path),
    })
}

fn measure_project(project: &Path) -> Vec<KindEntry> {
    let mut found = Vec::new();
    for def in KINDS {
        let mut dirs = Vec::new();
        for name in def.dirs {
            let candidate = project.join(name);
            if let Some(entry) = measure_dir(&candidate) {
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

fn plan_projects(args: &ProjectsArgs, root: &Path) -> anyhow::Result<Vec<ProjectPlan>> {
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

    let mut plans = Vec::new();
    for project in discover_projects(root, args.project.as_deref()) {
        let name = project
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| project.display().to_string());
        let mut kinds = Vec::new();
        let mut excluded = Vec::new();
        for entry in measure_project(&project) {
            if !only.is_empty() && !only.contains(entry.kind) {
                continue;
            }
            if except.contains(entry.kind) {
                continue;
            }
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

/// One prompt for the whole run after the plan has been shown. Piped output
/// auto-confirms so scripts never hang; a TTY requires `--yes` or an explicit
/// `y`.
fn confirmed(plans: &[ProjectPlan], total: u64, yes: bool) -> bool {
    if yes || !is_tty() {
        return true;
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
    apply: bool,
    plans: &[ProjectPlan],
    removed: Option<(usize, u64, &[String])>,
) -> Result<i32> {
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
        "apply": apply,
        "projects": projects,
    });
    let (removed_count, freed, errors) = removed.unwrap_or((0, 0, &[]));
    document["removed"] = json!(removed_count);
    document["freed"] = json!(freed);
    document["errors"] = json!(errors);
    println!("{}", serde_json::to_string_pretty(&document)?);
    Ok(0)
}

fn print_plan(root: &Path, plans: &[ProjectPlan], total: u64, include_unsafe: bool) {
    ui::section("tend", Some("projects"));
    ui::kv("root", &tildify(root));
    ui::kv(
        "projects",
        &format!(
            "{} with artifacts{}",
            plans.len(),
            if include_unsafe {
                " (including unsafe)"
            } else {
                ""
            }
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
    if !root.is_dir() {
        if args.json {
            let document = json!({
                "path": root.display().to_string(),
                "apply": args.apply,
                "projects": [],
                "removed": 0,
                "freed": 0,
                "errors": [format!("not a directory: {}", root.display())],
            });
            println!("{}", serde_json::to_string_pretty(&document)?);
        } else {
            ui::say_fail(&format!("not a directory: {}", tildify(&root)));
        }
        return Ok(1);
    }

    let plans = plan_projects(&args, &root)?;
    let actionable: Vec<ProjectPlan> = plans
        .into_iter()
        .filter(|plan| !plan.kinds.is_empty())
        .collect();
    let total: u64 = actionable.iter().map(|plan| plan.total).sum();

    if args.json {
        if !args.apply {
            emit_json(&root, false, &actionable, None)?;
            return Ok(0);
        }
        if actionable.is_empty() {
            emit_json(&root, true, &actionable, Some((0, 0, &[])))?;
            return Ok(0);
        }
        if !confirmed(&actionable, total, args.yes) {
            emit_json(&root, true, &actionable, Some((0, 0, &[])))?;
            eprintln!("cancelled, nothing was removed");
            return Ok(1);
        }
        let (removed, freed, errors) = delete_all(&actionable, true);
        let code = if errors.is_empty() { 0 } else { 1 };
        emit_json(&root, true, &actionable, Some((removed, freed, &errors)))?;
        return Ok(code);
    }

    print_plan(&root, &actionable, total, args.all);
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
    if !confirmed(&actionable, total, args.yes) {
        ui::say_fail("cancelled, nothing was removed");
        return Ok(1);
    }

    let (removed, freed, errors) = delete_all(&actionable, false);
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
            only: None,
            except: None,
            all: false,
            project: None,
            apply: false,
            yes: true,
            json: false,
        }
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
        let mut found = discover_projects(tmp.path(), None);
        found.sort();
        let names: Vec<String> = found
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["rust-app".to_string(), "web".to_string()]);
    }

    #[test]
    fn only_filter_selects_a_subset() {
        let tmp = fixture();
        let args = ProjectsArgs {
            only: Some("target".to_string()),
            ..base_args(tmp.path())
        };
        let plans = plan_projects(&args, tmp.path()).unwrap();
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
        let plans = plan_projects(&args, tmp.path()).unwrap();
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
        let without_all = plan_projects(&base_args(root), root).unwrap();
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
        let with_all = plan_projects(&args, root).unwrap();
        let actionable: Vec<&ProjectPlan> = with_all
            .iter()
            .filter(|plan| !plan.kinds.is_empty())
            .collect();
        assert_eq!(actionable.len(), 1);
        assert!(actionable[0].kinds.iter().any(|entry| entry.kind == "venv"));
    }
}
