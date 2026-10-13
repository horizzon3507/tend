use anyhow::Result;
use clap::{Args, CommandFactory, Parser, Subcommand};

use crate::commands;
use crate::ui;

#[derive(Parser, Debug)]
#[command(
    name = "tend",
    version,
    about = "the caretaker of your machine",
    long_about = "Tend the machine: scan dead weight, clean safely, back up dotfiles, and watch resources.\n\nSafety:\n  clean only deletes with --apply.\n  backup restore writes to a fresh directory unless you pass --in-place.",
    after_help = "Examples:\n  tend status\n  tend scan --depth 3\n  tend clean --apply\n  tend backup\n  tend watch --once",
    subcommand_required = false,
    arg_required_else_help = false
)]
pub struct Cli {
    /// disable colors
    #[arg(long = "no-color", global = true)]
    pub no_color: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// overview: disk, cleanup, backup, and links
    Status(StatusArgs),
    /// full scan: biggest directories, junk, repos, and links
    Scan(ScanArgs),
    /// remove caches and builds (dry run by default)
    Clean(CleanArgs),
    /// snapshot dotfiles, with rotation
    Backup(BackupArgs),
    /// live CPU, memory, and disk monitor
    Watch(WatchArgs),
    /// show the config file
    Config(ConfigArgs),
    /// clean build artifacts across the projects in a directory
    Projects(crate::commands::projects::ProjectsArgs),
}

#[derive(Args, Debug, Clone)]
pub struct CommonArgs {
    /// change the scanned root, repeatable
    #[arg(long = "root", value_name = "PATH")]
    pub roots: Vec<String>,

    /// scan depth
    #[arg(long, value_name = "N")]
    pub depth: Option<usize>,

    /// JSON output, for scripts
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct StatusArgs {
    #[command(flatten)]
    pub common: CommonArgs,
}

#[derive(Args, Debug)]
pub struct ScanArgs {
    #[command(flatten)]
    pub common: CommonArgs,

    /// days without a commit before a repo counts as stale
    #[arg(long = "stale-days", value_name = "N")]
    pub stale_days: Option<i64>,

    /// max scan items
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,

    /// how many targets to show
    #[arg(long, value_name = "N")]
    pub top: Option<usize>,
}

#[derive(Args, Debug)]
pub struct CleanArgs {
    #[command(flatten)]
    pub common: CommonArgs,

    /// max scan items
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,

    /// include unsafe targets (dist, build, .venv)
    ///
    /// Removes regenerable build artifacts and caches: node_modules, target/,
    /// .next, __pycache__, .turbo, test and linter caches, and other entries
    /// listed by the target table. Everything it deletes can be rebuilt
    /// (npm install / cargo build restore them). Tool installs
    /// (~/.bun/install, ~/.cargo/registry, ~/.local/opt, ~/.npm, ~/.nvm,
    /// go/pkg/mod, pnpm, plugins) are never listed and never touched.
    /// Dry run by default; only --apply deletes.
    #[arg(long, verbatim_doc_comment)]
    pub all: bool,

    /// actually delete (dry run by default)
    ///
    /// Removes regenerable build artifacts and caches: node_modules, target/,
    /// .next, __pycache__, .turbo, test and linter caches, and other entries
    /// listed by the target table. Everything it deletes can be rebuilt
    /// (npm install / cargo build restore them). Tool installs
    /// (~/.bun/install, ~/.cargo/registry, ~/.local/opt, ~/.npm, ~/.nvm,
    /// go/pkg/mod, pnpm, plugins) are never listed and never touched.
    #[arg(long, verbatim_doc_comment)]
    pub apply: bool,

    /// skip the confirmation prompt
    #[arg(long)]
    pub yes: bool,
}

#[derive(Args, Debug)]
pub struct WatchArgs {
    #[command(flatten)]
    pub common: CommonArgs,

    /// milliseconds between samples (minimum 250)
    #[arg(long, value_name = "MS")]
    pub interval: Option<u64>,

    /// take a single sample
    #[arg(long)]
    pub once: bool,

    /// samples before exiting
    #[arg(long, value_name = "N")]
    pub count: Option<u64>,
}

#[derive(Args, Debug)]
pub struct ConfigArgs {
    /// write the file with default values
    #[arg(long)]
    pub init: bool,

    /// print only the file path
    #[arg(long)]
    pub path: bool,
}

#[derive(Args, Debug)]
pub struct BackupArgs {
    #[command(subcommand)]
    pub action: Option<BackupAction>,

    /// extra path to store, repeatable (default: the ones in config)
    #[arg(long, value_name = "PATH", global = true)]
    pub path: Vec<String>,

    /// how many snapshots to keep
    #[arg(long, value_name = "N", global = true)]
    pub keep: Option<usize>,

    /// where to store snapshots
    #[arg(long, value_name = "DIR", global = true)]
    pub dest: Option<String>,

    /// restore destination (default: ~/tend-restore-<id>)
    #[arg(long, value_name = "DIR", global = true)]
    pub to: Option<String>,

    /// restore over the current files
    #[arg(long = "in-place", global = true)]
    pub in_place: bool,

    /// show what would be restored, without writing
    #[arg(long = "dry-run", global = true)]
    pub dry_run: bool,

    /// ignore the backup size limit
    #[arg(long, global = true)]
    pub force: bool,

    /// JSON output, for scripts
    #[arg(long, global = true)]
    pub json: bool,
}

#[derive(Subcommand, Debug)]
pub enum BackupAction {
    /// create a snapshot (default)
    Create,
    /// list stored snapshots
    List,
    /// restore a snapshot (never overwrites by default)
    Restore {
        /// id, id prefix, or "latest"
        reference: Option<String>,
    },
}

pub fn run(cli: Cli) -> Result<i32> {
    ui::init(cli.no_color);

    let Some(command) = cli.command else {
        let mut app = Cli::command();
        app.print_long_help()?;
        println!();
        return Ok(0);
    };

    match command {
        Command::Status(args) => commands::status::run(args),
        Command::Scan(args) => commands::scan::run(args),
        Command::Clean(args) => commands::clean::run(args),
        Command::Backup(args) => commands::backup::run(args),
        Command::Watch(args) => commands::watch::run(args),
        Command::Config(args) => commands::config::run(args),
        Command::Projects(args) => commands::projects::run(args),
    }
}
