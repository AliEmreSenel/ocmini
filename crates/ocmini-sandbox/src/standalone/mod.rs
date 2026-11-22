//! The `ocmini-sandbox` executable: run one command under a sandbox preset
//! without ocmini sessions, tools or a database.
use crate::{cli::SandboxArgs, config, refusal, REFUSAL};
use clap::Parser;
use std::{ffi::OsString, path::PathBuf, process::Command};

#[cfg(target_os = "linux")]
mod supervisor;

#[derive(Parser)]
#[command(version, about = "Run a command in the ocmini permission sandbox")]
struct Cli {
    /// Select a preset (default: OCMINI_SANDBOX_PRESET or read-only)
    #[arg(long = "sandbox-preset", visible_alias = "preset", value_name = "NAME")]
    preset: Option<String>,
    /// Working directory, also used for ${workspace}
    #[arg(short = 'C', long, value_name = "DIR")]
    directory: Option<PathBuf>,
    /// Preset directory (default: $XDG_CONFIG_HOME/opencode/sandbox/presets)
    #[arg(long, value_name = "DIR")]
    presets_dir: Option<PathBuf>,
    #[command(flatten)]
    sandbox: SandboxArgs,
    /// Program and arguments to execute directly; use -- before the program
    #[arg(required = true, last = true, value_name = "COMMAND", num_args = 1..)]
    command: Vec<OsString>,
}

fn run(cli: Cli) -> Result<i32, String> {
    let directory = match cli.directory {
        Some(directory) => directory,
        None => std::env::current_dir().map_err(refusal)?,
    }
    .canonicalize()
    .map_err(refusal)?;
    let context = config::PathContext::current(&directory).map_err(refusal)?;
    let folder = cli
        .presets_dir
        .unwrap_or_else(|| context.config_home.join("opencode/sandbox/presets"));
    config::initialize(&folder).map_err(refusal)?;
    let selection = cli
        .preset
        .or_else(|| {
            std::env::var("OCMINI_SANDBOX_PRESET")
                .ok()
                .filter(|value| !value.is_empty())
        })
        .unwrap_or_else(|| "read-only".into());
    let policy = config::load_with_overrides(
        &folder,
        &selection,
        &context,
        &cli.sandbox.overrides().map_err(refusal)?,
    )
    .map_err(refusal)?;
    let Some(policy) = policy else {
        let mut command = Command::new(&cli.command[0]);
        command.args(&cli.command[1..]).current_dir(directory);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            let error = command.exec();
            eprintln!(
                "ocmini-sandbox: {}: {error}",
                cli.command[0].to_string_lossy()
            );
            return Ok(if error.kind() == std::io::ErrorKind::NotFound {
                127
            } else {
                126
            });
        }
        #[cfg(not(unix))]
        return command
            .status()
            .map(|s| s.code().unwrap_or(1))
            .map_err(|e| e.to_string());
    };
    #[cfg(target_os = "linux")]
    return supervisor::run(policy, directory, &cli.command);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = policy;
        Err(refusal("sandbox requires Linux"))
    }
}

/// Entry point of the `ocmini-sandbox` binary.
pub fn main() -> ! {
    #[cfg(target_os = "linux")]
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("__command")) {
        let result = supervisor::command_main(std::env::args_os().skip(2).collect());
        exit(result);
    }
    exit(run(Cli::parse()));
}

fn exit(result: Result<i32, String>) -> ! {
    match result {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("ocmini-sandbox: {error}");
            std::process::exit(REFUSAL);
        }
    }
}
