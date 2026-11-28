//! End-to-end runs of the compiled binary in a scratch HOME, so the trash flow
//! is covered the way a user actually hits it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn tend(home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_tend"))
        .args(args)
        .env("HOME", home)
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .expect("run tend")
}

fn scratch() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let proj = home.join("proj");
    fs::create_dir_all(proj.join("target/debug")).unwrap();
    fs::create_dir_all(proj.join("node_modules/pkg")).unwrap();
    fs::write(proj.join("target/debug/blob.o"), "x".repeat(4096)).unwrap();
    fs::write(proj.join("node_modules/pkg/a.js"), "y".repeat(2048)).unwrap();
    (tmp, home)
}

fn trash_items(home: &Path) -> PathBuf {
    home.join(".local/share/tend/trash/items")
}

#[test]
fn apply_trashes_then_restore_then_empty() {
    let (_tmp, home) = scratch();
    let proj = home.join("proj");

    let out = tend(
        &home,
        &[
            "clean",
            "--apply",
            "--yes",
            "--root",
            proj.to_str().unwrap(),
        ],
    );
    assert!(
        out.status.success(),
        "clean: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!proj.join("target").exists(), "target left the project");
    let items: Vec<_> = fs::read_dir(trash_items(&home)).unwrap().collect();
    assert_eq!(items.len(), 2, "both directories are in the store");

    let out = tend(&home, &["trash", "restore", "latest"]);
    assert!(
        out.status.success(),
        "restore: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let restored = ["target", "node_modules"]
        .iter()
        .filter(|name| proj.join(name).exists())
        .count();
    assert_eq!(restored, 1, "exactly one directory came back");

    let out = tend(&home, &["trash", "empty", "--yes"]);
    assert!(
        out.status.success(),
        "empty: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let items: Vec<_> = fs::read_dir(trash_items(&home)).unwrap().collect();
    assert_eq!(items.len(), 0, "the store is empty");
}

#[test]
fn purge_deletes_without_touching_the_trash() {
    let (_tmp, home) = scratch();
    let proj = home.join("proj");

    let out = tend(
        &home,
        &[
            "clean",
            "--apply",
            "--purge",
            "--yes",
            "--root",
            proj.to_str().unwrap(),
        ],
    );
    assert!(out.status.success());
    assert!(!proj.join("target").exists());
    assert!(
        !trash_items(&home).exists() || fs::read_dir(trash_items(&home)).unwrap().next().is_none(),
        "purge leaves no payload in the store"
    );
}

#[test]
fn restore_refuses_to_overwrite_an_existing_path() {
    let (_tmp, home) = scratch();
    let proj = home.join("proj");

    let out = tend(
        &home,
        &[
            "clean",
            "--apply",
            "--yes",
            "--root",
            proj.to_str().unwrap(),
        ],
    );
    assert!(out.status.success());
    fs::create_dir_all(proj.join("target")).unwrap();
    fs::write(proj.join("target/new.o"), "fresh").unwrap();

    let out = tend(&home, &["trash", "restore", "latest"]);
    // latest is node_modules, which restores fine; restore it, then target must fail
    if out.status.success() {
        let out = tend(&home, &["trash", "restore", "latest"]);
        assert!(
            !out.status.success(),
            "second restore must refuse the collision"
        );
    }
    assert_eq!(
        fs::read_to_string(proj.join("target/new.o")).unwrap(),
        "fresh",
        "the existing directory was not clobbered"
    );
}

#[test]
fn empty_without_yes_on_a_piped_stdin_keeps_the_trash() {
    let (_tmp, home) = scratch();
    let proj = home.join("proj");

    let out = tend(
        &home,
        &[
            "clean",
            "--apply",
            "--yes",
            "--root",
            proj.to_str().unwrap(),
        ],
    );
    assert!(out.status.success());

    let out = tend(&home, &["trash", "empty"]);
    assert!(!out.status.success(), "non-interactive empty is refused");
    let items: Vec<_> = fs::read_dir(trash_items(&home)).unwrap().collect();
    assert_eq!(items.len(), 2, "nothing was deleted");
}
