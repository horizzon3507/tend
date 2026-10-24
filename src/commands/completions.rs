use anyhow::Result;
use clap::CommandFactory;

use crate::cli::{Cli, CompletionsArgs};

pub fn run(args: CompletionsArgs) -> Result<i32> {
    clap_complete::generate(
        args.shell,
        &mut Cli::command(),
        "tend",
        &mut std::io::stdout(),
    );
    Ok(0)
}
