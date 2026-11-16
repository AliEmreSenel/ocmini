pub use ocmini_core::*;
mod picker;
pub mod run;
pub mod tui;
use clap::Args;
fn parse_model(value: &str) -> Result<types::ModelRef, String> {
    types::ModelRef::parse(value)
}
#[derive(Args, Default)]
#[group(id = "tui_options")]
pub struct TuiArgs {
    #[arg(long, value_name = "PROVIDER/MODEL", value_parser = parse_model)]
    pub model: Option<types::ModelRef>,
    #[arg(long, value_name = "DIR")]
    pub directory: Option<String>,
    /// Resume the latest top-level session in this directory
    #[arg(short = 'c', long = "continue", conflicts_with = "session")]
    pub continue_last: bool,
    /// Resume a session by ID, using its saved directory
    #[arg(short = 's', long, value_name = "ID")]
    pub session: Option<String>,
    /// Pick a saved session interactively (search and sort)
    #[arg(
        short = 'r',
        long,
        conflicts_with = "session",
        conflicts_with = "continue_last"
    )]
    pub resume: bool,
}

#[derive(Args, Default)]
pub struct RunArgs {
    /// Prompt text; piped standard input is appended (like `opencode run`)
    pub message: Vec<String>,
    /// Resume the latest top-level session in this directory
    #[arg(short = 'c', long = "continue", conflicts_with = "session")]
    pub continue_last: bool,
    /// Resume a session by ID, using its saved directory
    #[arg(short = 's', long, value_name = "ID")]
    pub session: Option<String>,
    /// Fork the resumed session instead of continuing it
    #[arg(long, requires = "session")]
    pub fork: bool,
    #[arg(short = 'm', long, value_name = "PROVIDER/MODEL", value_parser = parse_model)]
    pub model: Option<types::ModelRef>,
    #[arg(long)]
    pub agent: Option<String>,
    #[arg(long)]
    pub variant: Option<String>,
    /// Title for a new session
    #[arg(long)]
    pub title: Option<String>,
    /// Attach a file (repeatable)
    #[arg(short = 'f', long = "file", value_name = "PATH")]
    pub file: Vec<String>,
    /// Output format: text prints the answer, json emits JSONL events
    #[arg(long, value_parser = ["text", "json"], default_value = "text")]
    pub format: String,
    /// Include chain of thought in the output
    #[arg(long)]
    pub thinking: bool,
    /// Directory for a new session (defaults to the current directory)
    #[arg(long, value_name = "DIR")]
    pub dir: Option<String>,
    /// Enter the interactive TUI after the turn
    #[arg(short = 'i', long)]
    pub interactive: bool,
    /// Allow tool permissions without asking
    #[arg(long)]
    pub auto: bool,
    /// Run a configured slash command with arguments
    #[arg(long)]
    pub command: Option<String>,
}

#[cfg(test)]
fn test_tempdir() -> std::io::Result<tempfile::TempDir> {
    tempfile::tempdir_in(std::env::temp_dir().canonicalize()?)
}
