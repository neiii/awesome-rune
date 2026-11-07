//! Maintains awesome-rune: generates the README and its activity charts, sorts and validates
//! the project list, and discovers repositories that depend on Rune.

mod discover;
mod github;
mod readme;
mod sort;
mod validate;

use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// The repository root, where projects.json and the README live.
pub fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// GITHUB_TOKEN from the environment or a local `.env`.
pub fn token() -> Option<String> {
    dotenvy::from_path(root().join(".env")).ok();
    std::env::var("GITHUB_TOKEN").ok().filter(|t| !t.is_empty())
}

#[derive(Parser)]
#[command(name = "awesome", about = "Maintains awesome-rune.")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generate README.md and the activity charts from projects.json and GitHub.
    Readme {
        /// Fail if README.md is not current instead of rewriting it.
        #[arg(long)]
        check: bool,
    },
    /// Sort projects.json by category, subcategory and name.
    Sort {
        /// Fail instead of rewriting an unsorted file.
        #[arg(long)]
        check: bool,
    },
    /// Validate projects.json and ignored.json against their schemas.
    Validate,
    /// Report repositories that depend on Rune but are neither listed nor ignored.
    Discover {
        /// Write the Markdown report here instead of stdout.
        #[arg(long)]
        output: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    let result = match Cli::parse().command {
        Command::Readme { check } => readme::run(check),
        Command::Sort { check } => sort::run(check),
        Command::Validate => validate::run(),
        Command::Discover { output } => discover::run(output),
    };
    match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
