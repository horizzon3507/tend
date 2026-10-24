# tend

Rust CLI with no runtime dependency that tends your machine: it measures
disk, finds dead weight, cleans safely, backs up dotfiles, and watches
CPU, memory, and disk.

Built for this machine's real case: a home near 90% usage, with **80.5 GiB**
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
npm (`tend@0.5.0`, a file watcher), PyPI, and crates.io. The Cargo package is
therefore named `tend-cli`; the binary is still `tend`. If you ever publish
to npm, use a scope (`@you/tend`).

## Install

From source. This repository lives at `~/Projects/tend`:

```sh
cd ~/Projects/tend
cargo build --release
install -m755 target/release/tend ~/.cargo/bin/tend
tend status
```

The Cargo package is `tend-cli` (version `0.2.0-beta`); the installed binary is
`tend`. Only `git` is required (for the repos part). Scanning, sizes, the tar
file, and monitoring are pure Rust: no external `du`, `df`, `find`, `tar`, or
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
| `tend check` | disk pressure check: prints usage and exits `2` when it reaches `--max-usage` (default 90%). `--quiet` prints nothing and only sets the exit code |
| `tend history` | shows what previous cleanup runs freed (`--limit`, default 20; `--json`) |
| `tend space` | reports space held by docker, journald, and the user cache (`~/.cache`). Read-only |
| `tend backup` | dotfile snapshot, with rotation |
| `tend backup list` | lists the stored snapshots |
| `tend backup verify [id]` | verifies a snapshot against its manifest checksum (`latest` by default) |
| `tend backup restore <id>` | restores a snapshot |
| `tend watch` | live CPU, memory, and disk monitor |
| `tend config` | shows the config file |
| `tend completions <shell>` | prints a completion script (bash, zsh, fish, elvish, powershell) |
| `tend man` | prints the man page as roff |

### Options that matter

```sh
tend scan --depth 3 --top 6        # faster, less noise
tend scan --limit 50               # list at most 50 findings; totals stay complete
tend clean --all                   # include unsafe targets (.venv, dist, build)
tend clean --apply                 # really delete, asks for confirmation
tend clean --apply --yes           # delete without asking
tend clean --older-than 30 --min-size 100MB   # only old, large targets
tend projects ~/code --depth 2     # search below ~/code as well as its immediate children
tend projects --apply                # delete, asks once: Remove <kinds> from <n> projects (X)? [y/N]
tend projects --apply --yes          # delete without asking; required when stdin is not a terminal
tend projects --only target,node_modules  # limit kinds (. prefix optional: --only .next)
tend projects --except venv --all    # --all includes unsafe kinds (dist, build, venv)
tend projects --project web          # only projects whose name contains "web"
tend projects --older-than 30 --min-size 100MB   # only old, large artifacts
tend projects --json                 # single document: {path, apply, projects:[{name, kinds:[{kind,bytes}], total}], removed, freed, errors}
tend backup --path ~/something --keep 10
tend backup verify latest
tend backup restore latest --to ~/inspect
tend backup restore latest --in-place   # snapshots current files first, then overwrites
tend check --max-usage 85           # exit 2 at or above 85%
tend check --quiet                  # usable from cron or a systemd timer
tend history --limit 5 --json
tend space --json                   # docker, journald, and ~/.cache usage
tend watch --once                  # one sample, good for scripts
tend scan --json                   # output for jq
tend completions bash > ~/.local/share/bash-completion/completions/tend
```

`projects --apply` requires `--yes` when stdin is not a terminal (piped or in
CI). It never assumes an answer: without a terminal and without `--yes`, it
cancels and deletes nothing.

`--limit` bounds how many findings are listed, never the totals: counts and
reclaimable bytes are computed over everything found, before truncation.

## Safety

A destructive operation never happens by accident:

- `clean` only deletes with `--apply`, and asks for confirmation in the terminal.
- Before deleting, the path must be a known target name AND sit inside a
  scanned root. Anything else is ignored.
- Scanning and cleanup never enter `~/.cache` looking for targets. `tend space`
  only reports its size; nothing under docker, journald, or `~/.cache` is
  deleted by any command.
- `~/.gradle`, `~/.m2`, and `~/.android` are protected: their contents are
  never listed as cleanup targets, no matter the depth.
- Targets that may hold authored work (`.venv`, `dist`, `build`) stay out of
  the default set and require `--all`.
- Tool installs (`~/.bun/install`, `~/.npm`, `~/.local/opt`, plugins) never
  enter the list, even when they are `node_modules`.
- `--older-than` and `--min-size` filter the plan before anything is removed;
  only targets that pass both are deleted.
- `backup restore` writes into a fresh directory (`~/tend-restore-<id>`),
  unless you pass `--in-place`. `--in-place` takes a pre-restore snapshot
  first, so the current files can be recovered.
- Backups over 2 GB are refused unless you pass `--force`.
- With `--json`, each run prints a single document (with `--apply`, the plan
  and the result ship together, never two JSON docs in a row).
- Totals are never capped by the display limit. The broken-link count is exact;
  at most 40 link paths are printed, and `linksCapped` says more exist.
- `projects` is dry-run by default; it deletes only with `--apply`, asks once
  with `Remove <kinds> from <n> projects (X)? [y/N]` on a TTY, requires `--yes`
  otherwise, and never follows symlinks. Unsafe kinds (`dist`, `build`, `venv`)
  are listed but skipped unless `--all` is passed. Tool installs use the same
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
`~/.local/opt`, `~/.omp/plugins`, `~/.local/share/pnpm`, `~/.gradle`, `~/.m2`,
`~/.android`.

On this machine's real scan that kept 24 entries out of the list, including
`~/.bun/install/global/node_modules` (4.2 GB),
`~/.local/opt/opencode-1/node_modules` (762 MB), and `~/.gradle` (155 MB).
`clean` reports how many it skipped and `--json` lists which.

Scanning and cleanup never enter `~/.cache` looking for targets, nor do user
files.

### Numbers on this machine

`tend scan` with the default (depth 4) finds **131 targets, 80.5 GiB**; with
`--depth 3` the list drops to 66 targets and 32.3 GiB, because deeper builds
fall out of reach.

Sizes come from disk blocks (`st_blocks`), matching `du -B1`, and each inode
is counted once, so hard links never inflate the total.

## Configuration

File at `~/.config/tend/config.json`, created on first run:

```json
{
  "roots": ["~"],
  "maxDepth": 4,
  "staleDays": 180,
  "exclude": [],
  "backup": {
    "paths": [
      "~/.bashrc",
      "~/.bash_profile",
      "~/.zshrc",
      "~/.profile",
      "~/.gitconfig",
      "~/.gitignore_global",
      "~/.tmux.conf",
      "~/.config/fish",
      "~/.config/nvim",
      "~/.config/hypr",
      "~/.config/kitty",
      "~/.config/alacritty",
      "~/.config/opencode",
      "~/.ssh",
      "~/dotfiles"
    ],
    "keep": 7,
    "dest": "~/.local/share/tend/backups"
  }
}
```

`exclude` takes path prefixes, and `~` expands to the home directory. Anything
at or below a listed prefix is skipped by scanning and cleanup:

```json
"exclude": ["~/Projects/legacy", "~/work/archive"]
```

Backup uses a curated list of light configs on purpose: the whole `~/.config`
is 26 GB on this machine, almost all of it app cache. Caches stay out of the
archive even inside the chosen paths.

## Proof it works

Verified on this machine, not claimed:

- **Equivalence with the TS version (v0.1.0 baseline)**, running both side by
  side: identical `disk` usage (322.84 GiB), the **131 targets** with zero path
  difference, the **8 biggest directories** in the same order, and **23
  protected installs**. v0.2.0 intentionally removes targets inside `~/.cache`
  and protects `~/.gradle`, so the current set is smaller.
- **v0.2.0 on this machine**: `scan --depth 4` reports **131 targets, 80.5
  GiB**, **24 protected installs**, and **104 broken links** (all counted; 40
  paths shown). `--depth 3` reports 66 targets and 32.3 GiB. `clean --json`
  lists zero targets under `~/.cache`, and `~/.gradle` appears only in
  `protectedPaths`. `scan --limit 1` reports the same `reclaimable` as an
  unlimited scan, with `totalFindings` intact.
- **v0.2.0 destructive safety**: `projects --apply` on piped stdin cancels
  with exit 1 and deletes nothing; `--yes` then removes the artifact and keeps
  `src/`. `clean --apply` on piped stdin also cancels. `check --quiet` exits 2
  at or above `--max-usage` and 0 below it. `backup verify` exits 0 for a fresh
  archive and 1 after one byte changes; `--in-place` restore creates a
  pre-restore snapshot; directory mtimes survive the round trip.
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
  and marked `~/.bun/install/global/node_modules` as protected. A real scan
  lists `~/.gradle` only as protected, and no `node_modules` under `~/.cache`
  is a finding.
- **Tests**: `cargo test` runs the test suite, including the backup cycle with
  `sha256` and 600 mode, rotation, the 2 GB size cap, the check that the block
  count matches `du -B1`, the cache and protected-directory rules, and the
  exact-totals/link-count behavior.
- **Every command**: `status`, `scan`, `clean`, `projects`, `check`,
  `history`, `space`, `completions`, `man`, `backup list`, `backup verify`,
  `watch --once`, `config`, `help`, and `version` run. `check` exits 2 above
  the threshold by design; missing root and unknown restore id exit 1.
  `cargo test` passes 76 tests.

## Limitations

- Scanning is parallel and I/O bound on very large trees; progress goes to
  stderr so `--json` keeps stdout clean.
- `git` is still called for repo state. Without `git` on PATH, the repos
  section stays empty instead of breaking.
- On restore, contents, permissions, and directory **mtimes** are reapplied;
  files keep their own mtime.
- Usage errors (unknown command or flag) exit with code 2, the `clap`
  default; the TS version exited 1. `tend check` also uses exit 2 to signal
  disk usage at or above the threshold.
- Directory names with newlines get no special treatment.
- The watcher reads `/proc`, so it is Linux-only.
- It never follows symlinks when measuring space (`-xdev` keeps everything
  on the same filesystem).
