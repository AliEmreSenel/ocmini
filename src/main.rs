mod auth;
mod bus;
mod config;
mod engine;
mod engine_impl;
mod mcp;
mod permission;
mod picker;
mod provider;
mod run;
mod server;
mod sessions;
mod store;
mod tools;
mod tui;
mod types;
mod usage_db;

use clap::{Args, Parser, Subcommand};

pub const OPENCODE_COMPAT_VERSION: &str = "1.19.3-ocmini.0.3.0";

#[derive(Parser)]
#[command(version = OPENCODE_COMPAT_VERSION, about = "A compact OpenCode-compatible coding runtime", args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    tui: TuiArgs,
}

#[derive(Args, Default)]
struct TuiArgs {
    #[arg(long, value_name = "PROVIDER/MODEL", value_parser = parse_model)]
    model: Option<types::ModelRef>,
    #[arg(long, value_name = "DIR")]
    directory: Option<String>,
    /// Resume the latest top-level session in this directory
    #[arg(short = 'c', long = "continue", conflicts_with = "session")]
    continue_last: bool,
    /// Resume a session by ID, using its saved directory
    #[arg(short = 's', long, value_name = "ID")]
    session: Option<String>,
    /// Pick a saved session interactively (search and sort)
    #[arg(
        short = 'r',
        long,
        conflicts_with = "session",
        conflicts_with = "continue_last"
    )]
    resume: bool,
}

#[derive(Args, Default)]
pub struct RunArgs {
    /// Prompt text; piped standard input is appended (like `opencode run`)
    message: Vec<String>,
    /// Resume the latest top-level session in this directory
    #[arg(short = 'c', long = "continue", conflicts_with = "session")]
    continue_last: bool,
    /// Resume a session by ID, using its saved directory
    #[arg(short = 's', long, value_name = "ID")]
    session: Option<String>,
    /// Fork the resumed session instead of continuing it
    #[arg(long, requires = "session")]
    fork: bool,
    #[arg(short = 'm', long, value_name = "PROVIDER/MODEL", value_parser = parse_model)]
    model: Option<types::ModelRef>,
    #[arg(long)]
    agent: Option<String>,
    #[arg(long)]
    variant: Option<String>,
    /// Title for a new session
    #[arg(long)]
    title: Option<String>,
    /// Attach a file (repeatable)
    #[arg(short = 'f', long = "file", value_name = "PATH")]
    file: Vec<String>,
    /// Output format: text prints the answer, json emits JSONL events
    #[arg(long, value_parser = ["text", "json"], default_value = "text")]
    format: String,
    /// Include chain of thought in the output
    #[arg(long)]
    thinking: bool,
    /// Directory for a new session (defaults to the current directory)
    #[arg(long, value_name = "DIR")]
    dir: Option<String>,
    /// Enter the interactive TUI after the turn
    #[arg(short = 'i', long)]
    interactive: bool,
    /// Allow tool permissions without asking
    #[arg(long)]
    auto: bool,
    /// Run a configured slash command with arguments
    #[arg(long)]
    command: Option<String>,
}

#[derive(Subcommand)]
enum Command {
    /// Start the interactive terminal chat (also the default)
    Tui(TuiArgs),
    /// Send one prompt and print the answer (like `opencode run`)
    Run(RunArgs),
    /// Inspect saved sessions
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    /// Start the HTTP/SSE server
    Serve {
        #[arg(long, default_value = "127.0.0.1", value_parser = parse_hostname)]
        hostname: String,
        #[arg(long, default_value_t = 4096)]
        port: u16,
    },
    /// List available models
    Models {
        provider: Option<String>,
        #[arg(long)]
        verbose: bool,
    },
    /// Inspect available agents
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },
    /// Inspect runtime resources
    Debug {
        #[command(subcommand)]
        command: DebugCommand,
    },
    /// Print the compatibility version
    Version,
}

#[derive(Subcommand)]
enum AgentCommand {
    List,
}

#[derive(Subcommand)]
enum DebugCommand {
    Skill,
}

#[derive(Subcommand)]
enum SessionCommand {
    /// List saved sessions, newest first
    List {
        #[arg(long, value_name = "DIR")]
        directory: Option<String>,
    },
}

fn parse_model(value: &str) -> Result<types::ModelRef, String> {
    let (provider, model) = value
        .split_once('/')
        .filter(|(provider, model)| !provider.is_empty() && !model.is_empty())
        .ok_or("model must be provider/model")?;
    Ok(types::ModelRef {
        provider_id: provider.into(),
        model_id: model.into(),
    })
}

fn parse_hostname(value: &str) -> Result<String, String> {
    let hostname = value.trim_start_matches('[').trim_end_matches(']');
    if hostname.is_empty() {
        return Err("hostname must not be empty".into());
    }
    Ok(hostname.to_owned())
}

async fn run(cli: Cli) -> Result<(), String> {
    match cli.command.unwrap_or(Command::Tui(cli.tui)) {
        Command::Tui(args) => tui::run(args).await,
        Command::Run(args) => run::run(args).await,
        Command::Session {
            command: SessionCommand::List { directory },
        } => {
            let directory = directory.as_deref().map(sessions::directory).transpose()?;
            let store = store::Store::open(&config::store_path(), &types::hex_id("ocm_"))?;
            for session in store.list_sessions(directory.as_deref())? {
                println!("{}\t{}\t{}", session.id, session.title, session.directory);
            }
            Ok(())
        }
        Command::Serve { hostname, port } => server::serve(hostname, port).await,
        Command::Version => {
            println!("{OPENCODE_COMPAT_VERSION}");
            Ok(())
        }
        command => {
            let config = config::Config::load()?;
            match command {
                Command::Models { provider, verbose } => {
                    let auth = auth::AuthStore::load();
                    let (config, errors) = provider::discover_models(&config, &auth).await;
                    if !errors.is_empty() && config.catalog().is_empty() {
                        return Err(errors.join("; "));
                    }
                    for error in errors {
                        eprintln!("ocmini: model discovery: {error}");
                    }
                    let inventory = server::provider_inventory(&config, &auth);
                    let connected = inventory["connected"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default();
                    for item in inventory["all"].as_array().into_iter().flatten() {
                        let id = item["id"].as_str().unwrap_or_default();
                        if provider.as_deref().is_some_and(|filter| filter != id)
                            || !connected.iter().any(|value| value.as_str() == Some(id))
                        {
                            continue;
                        }
                        if let Some(models) = item["models"].as_object() {
                            for (model_id, model) in models {
                                println!("{id}/{model_id}");
                                if verbose {
                                    println!(
                                        "{}",
                                        serde_json::to_string(model)
                                            .map_err(|error| error.to_string())?
                                    );
                                }
                            }
                        }
                    }
                }
                Command::Agent {
                    command: AgentCommand::List,
                } => {
                    let agents = server::agents_json(&config);
                    for agent in agents.as_array().into_iter().flatten() {
                        println!(
                            "{} ({})",
                            agent["name"].as_str().unwrap_or_default(),
                            agent["mode"].as_str().unwrap_or("all")
                        );
                        println!("{}", agent["permission"]);
                    }
                }
                Command::Debug {
                    command: DebugCommand::Skill,
                } => {
                    let directory = std::env::current_dir().map_err(|error| error.to_string())?;
                    println!(
                        "{}",
                        server::skills_json_for(&directory.to_string_lossy(), &config)
                    );
                }
                _ => unreachable!("execution commands are handled above"),
            }
            Ok(())
        }
    }
}

fn main() {
    let cli = Cli::try_parse().unwrap_or_else(|error| {
        if error.kind() == clap::error::ErrorKind::DisplayVersion {
            // T3's version probe expects the bare compatibility version.
            println!("{OPENCODE_COMPAT_VERSION}");
            std::process::exit(0);
        }
        error.exit()
    });
    // A single client mostly waits on providers and tools. Drive async I/O on
    // the main thread; blocking jobs still use Tokio's separate blocking pool.
    // Keep an explicit opt-in for instances serving CPU-heavy concurrent work.
    let mut runtime = if std::env::var_os("TOKIO_WORKER_THREADS").is_some() {
        tokio::runtime::Builder::new_multi_thread()
    } else {
        tokio::runtime::Builder::new_current_thread()
    };
    let result = runtime
        .enable_all()
        .build()
        .map_err(|error| format!("Cannot start runtime: {error}"))
        .and_then(|runtime| runtime.block_on(run(cli)));
    // Background shell jobs outlive their tool call; they end with the runtime.
    tools::stop_background_jobs();
    if let Err(error) = result {
        eprintln!("ocmini: {error}");
        std::process::exit(1);
    }
}
