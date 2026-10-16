# tend

Rust CLI with no runtime dependency that tends your machine: it measures
disk, finds dead weight, cleans safely, backs up dotfiles, and watches
CPU, memory, and disk.

Built for this machine's real case: a home near 90% usage, with **88.7 GiB**
of recyclable build spread across **131 directories**.

## Name

`tend` as in "tend the machine": care for it the way you tend a garden,
no drama and nothing destroyed. The tagline stays the same, "the caretaker
of your machine", which is exactly what it is.

It used to be called `zelador`. Both state directories were migrated and the
2026-10-16 backup was preserved with the manifest fixed. The old alias was
removed: today only `tend` exists on PATH.

Name check on 2026-10-16: `tend` is free in the disk-cleanup space (no disk
janitor uses that name), but taken in registries by tools with other jobs:
npm (`tend@0.5.0`, a file watcher), PyPI, and crates.io. If you ever publish
to npm, use a scope (`@you/tend`).

## Install

```sh
cd ~/tend
cargo build --release
install -m755 target/release/tend ~/.cargo/bin/tend
tend status
```

Only `git` is required (for the repos part). Scanning, sizes, the tar file,
and monitoring are pure Rust: no external `du`, `df`, `find`, `tar`, or
`zstd`.

It was Bun + TypeScript until the Rust rewrite. The old code is still in git
history (`git show ed9e552:src/cli.ts`).

## Commands

| Command | What it does |
| --- | --- |
| `tend status` | overview: disk, reclaimable space, backup, and links |
| `tend scan` | full scan: biggest directories, junk, repos, broken links |
| `tend clean` | removes caches and builds. Dry run by default |
| `tend projects [PATH]` | per-project artifact cleanup: finds projects under PATH (default `.`), lists `target/`, `node_modules/`, `.next/`, `dist/`, `build/`, `__pycache__/`, `.venv`/`venv`. Dry run by default |
| `tend backup` | dotfile snapshot, with rotation |
| `tend backup list` | lists the stored snapshots |
| `tend backup restore <id>` | restores a snapshot |
| `tend watch` | live CPU, memory, and disk monitor |
| `tend config` | shows the config file |

### Options that matter

```sh
tend scan --depth 3 --top 6        # faster, less noise
tend clean --all                   # include unsafe targets (.venv, dist, build)
tend clean --apply                 # really delete, asks for confirmation
tend clean --apply --yes           # delete without asking
tend projects ~/code                 # plan for all projects under ~/code, delete nothing
tend projects --apply                # delete, asks once: Remove <kinds> from <n> projects (X)? [y/N]
tend projects --apply --yes          # delete without asking (also auto-yes when piped)
tend projects --only target,node_modules  # limit kinds (. prefix optional: --only .next)
tend projects --except venv --all    # --all includes unsafe kinds (dist, build, venv)
tend projects --project web          # only projects whose name contains "web"
tend projects --json                 # single document: {path, apply, projects:[{name, kinds:[{kind,bytes}], total}], removed, freed, errors}
tend backup --path ~/something --keep 10
tend backup restore latest --to ~/inspect
tend backup restore latest --in-place   # overwrite the current files
tend watch --once                  # one sample, good for scripts
tend scan --json                   # output for jq
```

## Safety

A destructive operation never happens by accident:

- `clean` only deletes with `--apply`, and asks for confirmation in the terminal.
- Before deleting, the path must be a known target name AND sit inside a
  scanned root. Anything else is ignored.
- Targets that may hold authored work (`.venv`, `dist`, `build`) stay out of
  the default set and require `--all`.
- Tool installs (`~/.bun/install`, `~/.npm`, `~/.local/opt`, plugins) never
  enter the list, even when they are `node_modules`.
- `backup restore` writes into a fresh directory (`~/tend-restore-<id>`),
  unless you pass `--in-place`.
- Backups over 2 GB are refused unless you pass `--force`.
- With `--json`, each run prints a single document (with `--apply`, the plan
  and the result ship together, never two JSON docs in a row).
- `projects` is dry-run by default; it deletes only with `--apply`, asks once
  with `Remove <kinds> from <n> projects (X)? [y/N]` on a TTY, and never
  follows symlinks. Unsafe kinds (`dist`, `build`, `venv`) are listed but
  skipped unless `--all` is passed. Tool installs use the same
  protected-path list as `clean` and are never touched.

## How it decides what is junk

Only regenerable targets qualify: `node_modules`, `target`, `__pycache__`,
`.next`, `.turbo`, test and lint caches. The rule is simple: if the build or
the package manager recreates it, it can go.

### Installs never qualify

A `node_modules` inside an install directory belongs to a tool you already
installed, not to a project. Deleting it breaks the command instead of saving
build time. These stay out, always:

`~/.bun/install`, `~/.nvm`, `~/.npm`, `~/.cargo/registry`, `go/pkg/mod`,
`~/.local/opt`, `~/.omp/plugins`, `~/.local/share/pnpm`.

On this machine's real scan that kept 23 entries out of the list, including
`~/.bun/install/global/node_modules` (4.2 GB) and
`~/.local/opt/opencode-1/node_modules` (762 MB). `clean` reports how many it
skipped and `--json` lists which.

`~/.cache` never enters as a direct target either, nor do user files.

### Numbers on this machine

`tend scan` with the default (depth 4) finds **131 targets, 88.7 GiB**; with
`--depth 3` the list drops to 65 targets and 44.1 GiB, because the worktree
builds under `~/.synara` fall out of reach.

Sizes come from disk blocks (`st_blocks`), matching `du -B1`, and each inode
is counted once, so hard links never inflate the total.

## Configuration

File at `~/.config/tend/config.json`, created on first run:

```json
{
  "roots": ["~"],
  "maxDepth": 4,
  "staleDays": 180,
  "backup": {
    "paths": ["~/.bashrc", "~/.config/nvim", "~/.ssh", "~/dotfiles"],
    "keep": 7,
    "dest": "~/.local/share/tend/backups"
  }
}
```

Backup uses a curated list of light configs on purpose: the whole `~/.config`
is 26 GB on this machine, almost all of it app cache. Caches stay out of the
archive even inside the chosen paths.

## Proof it works

Verified on this machine, not claimed:

- **Equivalence with the TS version**, running both side by side: identical
  `disk` usage (322.84 GiB), the **131 targets** with zero path difference,
  the **8 biggest directories** in the same order, **40 broken links**, and
  **23 protected installs**. With `clean --depth 3`, 65 targets and
  44.13 GiB against 44.12 GiB from the TS version, with 62 of the 65 sizes
  identical byte for byte.
- **Archive interop**: system `tar` reads the `.tar.zst` written by Rust, and
  Rust reads and restores a `.tar.zst` written by system `tar`. Both tests run
  under `cargo test`.
- **Cross restore**: the real backup created by the TS version
  (`backup-20261016-183257062.tar.zst`, 759 items) was listed and restored by
  the Rust binary; `sha256sum` of `.gitconfig` matches the original,
  `diff -r` of `.bashrc`, `.profile`, and `.config/nvim` comes back identical,
  and the `600` permissions on keys under `~/.ssh` survive.
- **Real cleanup in a sandbox**: `clean --apply --yes` removed `node_modules`
  and `target` and left `src/main.rs` and `dist/` intact; `--all` also removes
  `dist` and `.venv`; without `--yes` and without a terminal, it cancels with
  code 1.
- **Protected installs**: with a fake HOME containing
  `~/.bun/install/global/node_modules`, the plan removed `~/proj/node_modules`
  and marked `~/.bun/install/global/node_modules` as protected.
- **Tests**: `cargo test` with 45+ tests, including the backup cycle with
  `sha256` and 600 mode, rotation, the 2 GB size cap, and the check that the
  block count matches `du -B1`.
- **Every command**: exit=0 everywhere (`status`, `scan`, `clean`,
  `projects`, `backup list`, `watch --once`, `config`, `help`, `version`),
  missing root and unknown restore id exit 1.

## Limitations

- Scanning is single-threaded (only repo inspection uses a thread pool).
  `tend scan` takes ~54 s on a big home and `clean`, without measuring the
  tree, ~15 s.
- `git` is still called for repo state. Without `git` on PATH, the repos
  section stays empty instead of breaking.
- On restore, contents and permissions are faithful, but directory **mtimes**
  are not reapplied at the end (files keep mtime).
- Usage errors (unknown command or flag) exit with code 2, the `clap`
  default; the TS version exited 1.
- Directory names with newlines get no special treatment.
- The watcher reads `/proc`, so it is Linux-only.
- It never follows symlinks when measuring space (`-xdev` keeps everything
  on the same filesystem).
