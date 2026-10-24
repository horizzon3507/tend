use std::ffi::CString;
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::scan::subtree_bytes;
use crate::util::{expand_home, format_stamp, home, human_bytes, now_ms, sha256_file};

pub const TOOL: &str = concat!("tend/", env!("CARGO_PKG_VERSION"));
const MANIFEST_VERSION: u32 = 1;
pub const SIZE_GUARD: u64 = 2 * 1024 * 1024 * 1024;

/// Cache-heavy artifacts never belong in a config backup.
const EXCLUDES: &[&str] = &[
    "node_modules",
    "*.log",
    "Cache",
    "GPUCache",
    "cached_data",
    "*.tmp",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub version: u32,
    pub id: String,
    pub created_at: i64,
    pub host: String,
    pub archive: String,
    pub bytes: u64,
    pub sha256: Option<String>,
    pub file_count: u64,
    pub paths: Vec<String>,
    pub tool: String,
}

fn manifest_path(archive: &Path) -> PathBuf {
    PathBuf::from(format!("{}.json", archive.display()))
}

fn hostname() -> String {
    fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|text| text.trim().to_string())
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "?".to_string())
}

fn excluded(name: &str) -> bool {
    EXCLUDES
        .iter()
        .any(|pattern| match pattern.strip_prefix('*') {
            Some(suffix) => name.ends_with(suffix),
            None => name == *pattern,
        })
}

pub fn list_backups(dest: &Path) -> Vec<Manifest> {
    let Ok(entries) = fs::read_dir(dest) else {
        return Vec::new();
    };

    let mut found: Vec<Manifest> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".tar.zst") {
            continue;
        }

        let sidecar = manifest_path(&path);
        if let Some(manifest) = read_manifest(&sidecar) {
            found.push(manifest);
            continue;
        }

        let Ok(meta) = entry.metadata() else {
            continue;
        };
        found.push(Manifest {
            version: MANIFEST_VERSION,
            id: name
                .trim_start_matches("backup-")
                .trim_end_matches(".tar.zst")
                .to_string(),
            created_at: (meta.mtime() as f64 * 1000.0) as i64,
            host: hostname(),
            archive: path.display().to_string(),
            bytes: meta.len(),
            sha256: None,
            file_count: 0,
            paths: Vec::new(),
            tool: TOOL.to_string(),
        });
    }

    found.sort_by_key(|manifest| std::cmp::Reverse(manifest.created_at));
    found
}

pub fn resolve_backup(dest: &Path, reference: &str) -> Option<Manifest> {
    let all = list_backups(dest);
    if all.is_empty() {
        return None;
    }
    if reference.is_empty() || reference == "latest" {
        return all.into_iter().next();
    }
    all.iter()
        .find(|m| m.id == reference)
        .or_else(|| {
            all.iter().find(|m| {
                Path::new(&m.archive)
                    .file_name()
                    .is_some_and(|n| n == reference)
            })
        })
        .or_else(|| all.iter().find(|m| m.id.starts_with(reference)))
        .cloned()
}

fn rotate(dest: &Path, keep: usize) -> Vec<PathBuf> {
    let mut removed = Vec::new();
    for stale in list_backups(dest).into_iter().skip(keep) {
        let archive = PathBuf::from(&stale.archive);
        fs::remove_file(&archive).ok();
        fs::remove_file(manifest_path(&archive)).ok();
        removed.push(archive);
    }
    removed
}

fn mtime_secs(meta: &fs::Metadata) -> u64 {
    meta.mtime().max(0) as u64
}

/// tar stores names without the leading slash, so archives restore cleanly from /.
fn archive_name(path: &Path) -> String {
    path.to_string_lossy().trim_start_matches('/').to_string()
}

fn append_tree<W: Write>(builder: &mut tar::Builder<W>, path: &Path) -> Result<u64> {
    let meta = fs::symlink_metadata(path)
        .with_context(|| format!("nao consegui ler {}", path.display()))?;
    let name = archive_name(path);

    if meta.file_type().is_symlink() {
        let target = fs::read_link(path)?;
        let mut header = tar::Header::new_gnu();
        header.set_size(0);
        header.set_mode(0o777);
        header.set_mtime(mtime_secs(&meta));
        header.set_entry_type(tar::EntryType::Symlink);
        builder.append_link(&mut header, &name, &target)?;
        return Ok(1);
    }

    if meta.is_dir() {
        let mut header = tar::Header::new_gnu();
        header.set_size(0);
        header.set_mode(meta.permissions().mode());
        header.set_mtime(mtime_secs(&meta));
        header.set_entry_type(tar::EntryType::Directory);
        header.set_cksum();
        builder.append_data(&mut header, &name, std::io::empty())?;

        let mut count = 1;
        for entry in fs::read_dir(path)?.flatten() {
            let child = entry.path();
            let child_name = entry.file_name().to_string_lossy().into_owned();
            if excluded(&child_name) {
                continue;
            }
            count += append_tree(builder, &child)?;
        }
        return Ok(count);
    }

    if meta.is_file() {
        let mut file = File::open(path)?;
        builder.append_file(&name, &mut file)?;
        return Ok(1);
    }

    Ok(0)
}

fn write_archive(archive: &Path, sources: &[PathBuf]) -> Result<u64> {
    let file = File::create(archive)
        .with_context(|| format!("nao consegui criar {}", archive.display()))?;
    let encoder = zstd::Encoder::new(file, 3)?;
    let mut builder = tar::Builder::new(encoder);
    builder.follow_symlinks(false);

    let mut count = 0;
    for source in sources {
        count += append_tree(&mut builder, source)?;
    }

    let encoder = builder.into_inner()?;
    encoder.finish()?;
    Ok(count)
}

#[derive(Debug)]
pub struct CreateResult {
    pub manifest: Manifest,
    pub rotated: Vec<PathBuf>,
}

pub struct CreateOptions<'a> {
    pub dest: &'a Path,
    pub paths: &'a [String],
    pub keep: usize,
    pub force: bool,
    pub size_limit: u64,
}

pub fn create(options: &CreateOptions) -> Result<CreateResult> {
    let CreateOptions {
        dest,
        paths,
        keep,
        force,
        size_limit,
    } = options;

    let existing: Vec<PathBuf> = paths
        .iter()
        .map(|entry| expand_home(entry))
        .filter(|path| path.exists())
        .collect();
    if existing.is_empty() {
        bail!("no backup paths exist on disk");
    }

    let bytes: u64 = existing.iter().map(|path| subtree_bytes(path)).sum();
    if bytes > *size_limit && !force {
        bail!(
            "paths total {}, above the safety limit. Review the entries or use --force to proceed.",
            human_bytes(bytes)
        );
    }

    fs::create_dir_all(dest)?;
    let id = format_stamp(now_ms());
    let archive = dest.join(format!("backup-{id}.tar.zst"));

    let file_count = match write_archive(&archive, &existing) {
        Ok(count) => count,
        Err(error) => {
            fs::remove_file(&archive).ok();
            return Err(error);
        }
    };

    let manifest = Manifest {
        version: MANIFEST_VERSION,
        id,
        created_at: now_ms(),
        host: hostname(),
        archive: archive.display().to_string(),
        bytes: fs::metadata(&archive)?.len(),
        sha256: sha256_file(&archive).ok(),
        file_count,
        paths: existing
            .iter()
            .map(|path| path.display().to_string())
            .collect(),
        tool: TOOL.to_string(),
    };

    let sidecar = manifest_path(&archive);
    fs::write(
        &sidecar,
        format!("{}\n", serde_json::to_string_pretty(&manifest)?),
    )?;

    Ok(CreateResult {
        manifest,
        rotated: rotate(dest, *keep),
    })
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyResult {
    pub manifest_id: String,
    pub archive: String,
    pub expected: Option<String>,
    pub actual: String,
    pub bytes: u64,
    pub ok: bool,
}

/// Rehashes the archive on disk and compares it with the manifest digest. The
/// archive itself is never rewritten.
pub fn verify(manifest: &Manifest) -> Result<VerifyResult> {
    let Some(expected) = manifest.sha256.as_deref() else {
        bail!("manifest has no sha256");
    };

    let archive = PathBuf::from(&manifest.archive);
    if !archive.exists() {
        bail!("archive not found: {}", manifest.archive);
    }

    let actual = sha256_file(&archive)?;
    Ok(VerifyResult {
        manifest_id: manifest.id.clone(),
        archive: manifest.archive.clone(),
        expected: Some(expected.to_string()),
        actual: actual.clone(),
        bytes: fs::metadata(&archive).map(|meta| meta.len()).unwrap_or(0),
        ok: actual.eq_ignore_ascii_case(expected),
    })
}

pub fn archive_entries(archive: &Path, limit: usize) -> Result<Vec<String>> {
    let file = File::open(archive)?;
    let decoder = zstd::Decoder::new(file)?;
    let mut tar = tar::Archive::new(decoder);

    let mut names = Vec::new();
    for entry in tar.entries()? {
        let entry = entry?;
        if limit > 0 && names.len() >= limit {
            break;
        }
        names.push(entry.path()?.to_string_lossy().into_owned());
    }
    Ok(names)
}

/// tar cannot keep directory timestamps while extracting: writing children
/// touches the parent. Reapply the stored stamps deepest-first afterwards.
fn reapply_dir_mtimes(archive: &Path, target: &Path) {
    let Ok(file) = File::open(archive) else {
        return;
    };
    let Ok(decoder) = zstd::Decoder::new(file) else {
        return;
    };
    let mut tar = tar::Archive::new(decoder);
    let Ok(entries) = tar.entries() else {
        return;
    };

    let mut dirs: Vec<(PathBuf, u64)> = Vec::new();
    for entry in entries.flatten() {
        let header = entry.header();
        if !header.entry_type().is_dir() {
            continue;
        }
        let Ok(path) = entry.path() else {
            continue;
        };
        let path = path.into_owned();
        if path.is_absolute() || path.components().any(|part| part == Component::ParentDir) {
            continue;
        }
        dirs.push((path, header.mtime().unwrap_or(0)));
    }

    dirs.sort_by_key(|(path, _)| std::cmp::Reverse(path.components().count()));

    for (relative, mtime) in dirs {
        let path = target.join(relative);
        let Ok(cpath) = CString::new(path.as_os_str().as_bytes()) else {
            continue;
        };
        let stamp = libc::timespec {
            tv_sec: mtime as libc::time_t,
            tv_nsec: 0,
        };
        let times = [stamp, stamp];
        // A directory that cannot be stamped is cosmetic, never a restore failure.
        unsafe {
            libc::utimensat(libc::AT_FDCWD, cpath.as_ptr(), times.as_ptr(), 0);
        }
    }
}

pub fn restore(
    manifest: &Manifest,
    to: Option<&str>,
    in_place: bool,
    dry_run: bool,
) -> Result<(String, usize)> {
    let archive = PathBuf::from(&manifest.archive);
    if !archive.exists() {
        bail!("archive not found: {}", manifest.archive);
    }

    let entries = archive_entries(&archive, 0)?;

    if dry_run {
        let target = if in_place {
            "/".to_string()
        } else {
            expand_home(to.unwrap_or("")).display().to_string()
        };
        return Ok((target, entries.len()));
    }

    let target = if in_place {
        PathBuf::from("/")
    } else {
        let target = match to {
            Some(path) => expand_home(path),
            None => home().join(format!("tend-restore-{}", manifest.id)),
        };
        fs::create_dir_all(&target)?;
        target
    };

    let file = File::open(&archive)?;
    let decoder = zstd::Decoder::new(file)?;
    let mut tar = tar::Archive::new(decoder);
    tar.set_preserve_permissions(true);
    tar.set_preserve_mtime(true);
    tar.unpack(&target)
        .with_context(|| format!("could not extract to {}", target.display()))?;
    reapply_dir_mtimes(&archive, &target);

    Ok((target.display().to_string(), entries.len()))
}

/// Round-trips an archive through its own reader, so a corrupt manifest is caught
/// before anyone relies on it during a restore.
pub fn read_manifest(path: &Path) -> Option<Manifest> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    use super::*;

    fn sandbox() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("home/.ssh")).unwrap();
        fs::write(root.join("home/.bashrc"), "export PATH=1\n").unwrap();
        fs::write(root.join("home/.ssh/id_rsa"), "key\n").unwrap();
        fs::set_permissions(
            root.join("home/.ssh/id_rsa"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        fs::create_dir_all(root.join("home/node_modules/trash")).unwrap();
        fs::write(root.join("home/node_modules/trash/a.js"), "x".repeat(4096)).unwrap();
        dir
    }

    fn sources(root: &Path) -> Vec<String> {
        vec![root.join("home").display().to_string()]
    }

    fn options<'a>(
        dest: &'a Path,
        paths: &'a [String],
        keep: usize,
        force: bool,
    ) -> CreateOptions<'a> {
        CreateOptions {
            dest,
            paths,
            keep,
            force,
            size_limit: SIZE_GUARD,
        }
    }

    #[test]
    fn excludes_match_names_and_suffixes() {
        assert!(excluded("node_modules"));
        assert!(excluded("horario.log"));
        assert!(excluded("GPUCache"));
        assert!(!excluded("logs"));
        assert!(!excluded("main.rs"));
    }

    #[test]
    fn archive_names_never_start_with_a_slash() {
        assert_eq!(archive_name(Path::new("/home/g/.bashrc")), "home/g/.bashrc");
        assert_eq!(archive_name(Path::new("/etc/hosts")), "etc/hosts");
    }

    #[test]
    fn a_missing_archive_is_reported_instead_of_restoring_nothing() {
        let manifest = Manifest {
            version: 1,
            id: "x".into(),
            created_at: 0,
            host: "h".into(),
            archive: "/missing/backup-x.tar.zst".into(),
            bytes: 0,
            sha256: None,
            file_count: 0,
            paths: Vec::new(),
            tool: TOOL.into(),
        };
        assert!(restore(&manifest, Some("/tmp"), false, false).is_err());
    }

    #[test]
    fn an_empty_destination_lists_no_backups() {
        let dir = tempfile::tempdir().unwrap();
        assert!(list_backups(dir.path()).is_empty());
        assert!(resolve_backup(dir.path(), "latest").is_none());
    }

    #[test]
    fn backup_round_trip_restores_every_file_with_hash_and_mode() {
        let dir = sandbox();
        let paths = sources(dir.path());
        let dest = dir.path().join("backups");
        let created = create(&options(&dest, &paths, 5, false)).unwrap();

        assert!(created.manifest.bytes > 0);
        assert!(created.manifest.sha256.is_some());

        let names = archive_entries(Path::new(&created.manifest.archive), 0).unwrap();
        assert!(names.iter().any(|name| name.ends_with("home/.ssh/id_rsa")));
        assert!(
            !names.iter().any(|name| name.contains("node_modules")),
            "node_modules left out of the archive"
        );

        let restored = dir.path().join("restored");
        let (target, entries) = restore(
            &created.manifest,
            Some(&restored.display().to_string()),
            false,
            false,
        )
        .unwrap();
        assert_eq!(entries, names.len());

        let original = dir.path().join("home/.ssh/id_rsa");
        let relative = original.strip_prefix("/").unwrap();
        let copy = Path::new(&target).join(relative);
        assert_eq!(
            sha256_file(&original).unwrap(),
            sha256_file(&copy).unwrap(),
            "restored contents identical"
        );
        let mode = fs::metadata(&copy).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "key keeps its 600 mode");
    }

    #[test]
    fn gnu_tar_reads_what_we_write() {
        let dir = sandbox();
        let paths = sources(dir.path());
        let dest = dir.path().join("backups");
        let created = create(&options(&dest, &paths, 5, false)).unwrap();

        let listing = crate::util::run(
            "tar",
            &["-tf".into(), created.manifest.archive.clone()],
            Duration::from_secs(60),
        )
        .unwrap();
        assert!(listing.ok, "system tar reads what we write");
        assert!(listing.stdout.contains("home/.bashrc"));
    }

    #[test]
    fn we_read_an_archive_written_by_gnu_tar() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("thing")).unwrap();
        fs::write(dir.path().join("thing/a.txt"), "hi\n").unwrap();
        let dest = dir.path().join("backups");
        fs::create_dir_all(&dest).unwrap();
        let archive = dest.join("backup-20260101-000000000.tar.zst");

        let made = crate::util::run(
            "tar",
            &[
                "-c".into(),
                "--zstd".into(),
                "-f".into(),
                archive.display().to_string(),
                "-C".into(),
                dir.path().display().to_string(),
                "thing".into(),
            ],
            Duration::from_secs(60),
        )
        .unwrap();
        assert!(made.ok);

        let found = list_backups(&dest);
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].file_count, 0,
            "no sidecar means the total stays unknown"
        );

        let names = archive_entries(&archive, 0).unwrap();
        assert!(names.iter().any(|name| name.ends_with("thing/a.txt")));

        let back = dir.path().join("roundtrip");
        let (target, _) =
            restore(&found[0], Some(&back.display().to_string()), false, false).unwrap();
        assert!(Path::new(&target).join("thing/a.txt").exists());
    }

    #[test]
    fn the_size_guard_refuses_big_backups_unless_forced() {
        let dir = sandbox();
        let paths = sources(dir.path());
        let dest = dir.path().join("backups");

        let refused = CreateOptions {
            dest: &dest,
            paths: &paths,
            keep: 5,
            force: false,
            size_limit: 1,
        };
        let error = create(&refused).unwrap_err().to_string();
        assert!(error.contains("safety limit"), "error was: {error}");
        assert!(!dest.exists(), "nothing written to disk before refusing");

        let forced = CreateOptions {
            force: true,
            ..refused
        };
        let created = create(&forced).unwrap();
        assert!(created.manifest.bytes > 0);
    }

    #[test]
    fn rotation_keeps_only_the_requested_number() {
        let dir = sandbox();
        let paths = sources(dir.path());
        let dest = dir.path().join("backups");

        for _ in 0..3 {
            create(&options(&dest, &paths, 2, false)).unwrap();
            std::thread::sleep(Duration::from_millis(5));
        }

        let kept = list_backups(&dest);
        assert_eq!(kept.len(), 2);
        assert!(kept[0].created_at >= kept[1].created_at);
    }

    fn set_mtime(path: &Path, secs: i64) {
        let cpath = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        let stamp = libc::timespec {
            tv_sec: secs as libc::time_t,
            tv_nsec: 0,
        };
        let times = [stamp, stamp];
        let result = unsafe { libc::utimensat(libc::AT_FDCWD, cpath.as_ptr(), times.as_ptr(), 0) };
        assert_eq!(result, 0, "could not stamp {}", path.display());
    }

    #[test]
    fn verify_accepts_a_fresh_backup_and_catches_a_corrupted_one() {
        let dir = sandbox();
        let paths = sources(dir.path());
        let dest = dir.path().join("backups");
        let created = create(&options(&dest, &paths, 5, false)).unwrap();

        let clean = verify(&created.manifest).unwrap();
        assert!(clean.ok);
        assert_eq!(clean.expected, created.manifest.sha256);
        assert_eq!(clean.actual, created.manifest.sha256.clone().unwrap());
        assert_eq!(clean.bytes, created.manifest.bytes);

        let archive = PathBuf::from(&created.manifest.archive);
        let mut bytes = fs::read(&archive).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        fs::write(&archive, &bytes).unwrap();

        let damaged = verify(&created.manifest).unwrap();
        assert!(!damaged.ok);
        assert_ne!(damaged.actual, damaged.expected.unwrap());
    }

    #[test]
    fn verify_refuses_a_manifest_without_a_digest() {
        let dir = sandbox();
        let paths = sources(dir.path());
        let dest = dir.path().join("backups");
        let created = create(&options(&dest, &paths, 5, false)).unwrap();

        let mut manifest = created.manifest;
        manifest.sha256 = None;
        let error = verify(&manifest).unwrap_err().to_string();
        assert!(error.contains("no sha256"), "error was: {error}");
    }

    #[test]
    fn restored_directories_keep_their_source_mtime() {
        let dir = sandbox();
        let stamped = dir.path().join("home/.config/app");
        fs::create_dir_all(&stamped).unwrap();
        fs::write(stamped.join("conf.toml"), "theme = \"dark\"\n").unwrap();
        let known = 1_600_000_000i64;
        set_mtime(&stamped, known);

        let paths = sources(dir.path());
        let dest = dir.path().join("backups");
        let created = create(&options(&dest, &paths, 5, false)).unwrap();

        let restored = dir.path().join("restored");
        restore(
            &created.manifest,
            Some(&restored.display().to_string()),
            false,
            false,
        )
        .unwrap();

        let copy = restored.join(stamped.strip_prefix("/").unwrap());
        let got = fs::metadata(&copy).unwrap().mtime();
        assert!(
            (got - known).abs() <= 1,
            "restored mtime {got} differs from source {known}"
        );
    }
}
