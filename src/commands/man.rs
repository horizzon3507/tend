use anyhow::Result;
use clap::CommandFactory;

use crate::cli::{Cli, ManArgs};

pub fn run(_args: ManArgs) -> Result<i32> {
    clap_mangen::Man::new(Cli::command()).render(&mut std::io::stdout())?;
    Ok(0)
}
