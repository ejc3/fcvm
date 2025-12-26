use std::io;

use clap::CommandFactory;
use clap_complete::generate;

use crate::cli::args::{Cli, CompletionsArgs};

/// Generate shell completions to stdout.
pub fn cmd_completions(args: CompletionsArgs) {
    let mut cmd = Cli::command();
    generate(args.shell, &mut cmd, "fcvm", &mut io::stdout());
}
