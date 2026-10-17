# Changelog

We follow [Semantic Versioning](https://semver.org/) and [Keep a Changelog](https://keepachangelog.com/). tend is a single CLI surface.

<details>
<summary>To see more about versioning, expand this.</summary>

Every version string starts with `v` (required), e.g. `v0.2.0-stable`, `v0.1.0-stable`.

Here the installable surface is **CLI** (`tend`). Other apps swap in their own names the same way — e.g. **GNOME**, **Desktop**, **Web** — whatever you actually ship.

| Part | What you install | Example |
| --- | --- | --- |
| **CLI** | `tend` in the terminal | `v0.1.0-beta` |

With one surface there is no `m` in the tag and no per-surface sections — just the version notes.

Each release heading is the version and date (`## v0.1.0-beta · 17/10/2026`); under it, a short summary ends with a plain sentence like: "This version was made for CLI with a beta release channel on 17/10/2026 (v0.1.0-beta)."

### What the channel suffix means

| Suffix | In plain words |
| --- | --- |
| **-alpha** | Very early. Expect missing pieces and lots of bugs. |
| **-beta** | Mostly there, but still rough. Fine to try; not the "official" install. |
| **-stable** | Ready for daily use. This is what we put on GitHub Releases and the AUR. |

We only call something **stable** when we mean it.

</details>

## v0.1.0-beta · 17/10/2026

First cut of `tend`, the caretaker CLI rewritten in Rust from the old `zelador` (Bun + TypeScript). This version was made for CLI with a beta release channel on 17/10/2026 (v0.1.0-beta).

- `tend status`, `scan`, `clean`, `backup`, `watch`, and `config`: the caretaker toolkit in one binary with no runtime dependency.
- Scanning and sizes in pure Rust (`st_blocks`, each inode counted once), matching `du -B1`; on this machine, 131 targets / 88.7 GiB at depth 4.
- Safe cleanup: `--apply` required, confirmation in the terminal, known target names inside scanned roots only, protected install paths never touched, `.venv`/`dist`/`build` out of the default set.
- `tend projects [PATH]`: lists regenerable artifacts (`target/`, `node_modules/`, `.next/`, `dist/`, `build/`, `__pycache__/`, `.venv`/`venv`) per project; dry run by default, `--apply` with one confirmation, `--only`/`--except`/`--project` filters, `--json` for scripts.
- Dotfile backups as `.tar.zst` with rotation, a 2 GB cap, restore into a fresh directory, and 600 permissions on keys preserved.
- Live CPU / memory / disk watcher via `/proc` (`--once` for scripts); Linux-only.
- Verified against the TS version side by side: identical disk, targets, and order; archive interop with system `tar` both ways; the 2026-10-16 backup restored by the Rust binary with matching `sha256`.
- 45+ tests under `cargo test`, the old alias `zelador` removed, and state directories migrated.
- Various other UI polish and small tweaks
