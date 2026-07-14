//! CLI argument parsing via clap.
//!
//! Supports subcommands (`extract`, `rebuild`, `restore`, `inspect`, `list`)
//! and a legacy auto-detect mode when a bare file path is given.

use clap::{Parser, Subcommand};

/// Extract and rebuild UKMM mod files to/from editable YAML and native BYML.
#[derive(Parser)]
#[command(
    name = "ukmm-extractool",
    version,
    about,
    long_about = None
)]
pub struct Cli {
    /// Force interface language (en or fr).
    #[arg(long, value_name = "LANG", global = true)]
    pub lang: Option<String>,

    /// Output directory (default: auto-detected workspace path).
    #[arg(short = 'o', long, value_name = "DIR", global = true)]
    pub output: Option<String>,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum Commands {
    /// Extract a mod or file to editable YAML / native BYML.
    Extract {
        /// Path to a UKMM mod ZIP, loose mod directory, or a single .byml/.sarc file.
        path: String,
    },

    /// Rebuild a mod from edited files in a workspace.
    Rebuild {
        /// Path to a workspace directory (defaults to current directory).
        path: Option<String>,
    },

    /// Restore a mod's original backup back to UKMM.
    Restore {
        /// Path to a workspace directory (defaults to current directory).
        path: Option<String>,
    },

    /// List available UKMM mods.
    List,
}

impl Cli {
    /// Parse CLI args; on failure or `--help` / `--version`, the process exits.
    pub fn parse_or_exit() -> Self {
        <Self as Parser>::parse()
    }

    // Legacy auto-detect is handled in main() by checking args before clap.
}
