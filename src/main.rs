use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use ocmini_core::*;
use ocmini_server::server;
use ocmini_tui::{run, tui, RunArgs, TuiArgs};

#[cfg(feature = "sandbox")]
mod sandbox_args;

#[derive(Parser)]
#[command(version = OPENCODE_COMPAT_VERSION, about = "A compact OpenCode-compatible coding runtime")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    tui: TuiArgs,
    /// Select a sandbox preset file (also OCMINI_SANDBOX_PRESET)
    #[arg(long, global = true, value_name = "NAME")]
    sandbox_preset: Option<String>,
    #[cfg(feature = "sandbox")]
    #[command(flatten)]
    sandbox: sandbox_args::SandboxArgs,
}

#[derive(Subcommand)]
enum Command {
    /// Start the interactive terminal chat (also the default)
    Tui {
        #[command(flatten)]
        args: TuiArgs,
        #[cfg(feature = "sandbox")]
        #[command(flatten)]
        sandbox: sandbox_args::SandboxArgs,
    },
    /// Send one prompt and print the answer (like `opencode run`)
    Run {
        #[command(flatten)]
        args: RunArgs,
        #[cfg(feature = "sandbox")]
        #[command(flatten)]
        sandbox: sandbox_args::SandboxArgs,
    },
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
        #[cfg(feature = "sandbox")]
        #[command(flatten)]
        sandbox: sandbox_args::SandboxArgs,
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

#[cfg(feature = "sandbox")]
impl Cli {
    fn merge_sandbox(&mut self) {
        let Some(
            Command::Tui { sandbox, .. }
            | Command::Run { sandbox, .. }
            | Command::Serve { sandbox, .. },
        ) = &mut self.command
        else {
            return;
        };
        self.sandbox.append(std::mem::take(sandbox));
    }
}

#[derive(Subcommand)]
enum AgentCommand {
    List,
}

#[derive(Subcommand)]
enum DebugCommand {
    Skill,
    /// Validate and inspect hook registrations without running commands
    Hooks {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum SessionCommand {
    /// List saved sessions, newest first
    List {
        #[arg(long, value_name = "DIR")]
        directory: Option<String>,
    },
}

fn parse_hostname(value: &str) -> Result<String, String> {
    let hostname = value.trim_start_matches('[').trim_end_matches(']');
    if hostname.is_empty() {
        return Err("hostname must not be empty".into());
    }
    Ok(hostname.to_owned())
}

async fn with_app<F: std::future::Future<Output = Result<(), String>>>(
    app: engine::App,
    run: impl FnOnce(engine::App) -> F,
) -> Result<(), String> {
    let runtime = app.hook_runtime.clone();
    let result = run(app).await;
    runtime.shutdown().await;
    result
}

async fn run(cli: Cli) -> Result<(), String> {
    #[cfg(feature = "sandbox")]
    let cli = {
        let mut cli = cli;
        cli.merge_sandbox();
        cli
    };
    let create_app = || {
        #[cfg(feature = "sandbox")]
        {
            let overrides = cli.sandbox.overrides().map_err(ocmini_sandbox::refusal)?;
            engine::App::new_with_sandbox_overrides(overrides)
        }
        #[cfg(not(feature = "sandbox"))]
        {
            engine::App::new()
        }
    };
    match cli.command.unwrap_or(Command::Tui {
        args: cli.tui,
        #[cfg(feature = "sandbox")]
        sandbox: Default::default(),
    }) {
        Command::Tui { args, .. } => with_app(create_app()?, |app| tui::run(app, args)).await,
        Command::Run { args, .. } => with_app(create_app()?, |app| run::run(app, args)).await,
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
        Command::Serve { hostname, port, .. } => {
            let app = create_app()?;
            #[cfg(feature = "sandbox")]
            app.sandbox.validate_cli_defaults(&app.config)?;
            with_app(app, |app| server::serve(app, hostname, port)).await
        }
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
                    let inventory = inventory::provider_inventory(&config, &auth);
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
                    let agents = inventory::agents_json(&config);
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
                        inventory::skills_json_for(&directory.to_string_lossy(), &config)
                    );
                }
                Command::Debug {
                    command: DebugCommand::Hooks { json },
                } => {
                    let directory = std::env::current_dir().map_err(|e| e.to_string())?;
                    let mut catalog = hooks::debug_config(&config, &directory.to_string_lossy())?;
                    if config::store_path().exists() {
                        catalog["deliveries"] =
                            serde_json::json!(store::Store::inspect_hook_deliveries(
                                &config::store_path(),
                                &directory.to_string_lossy()
                            )?);
                    }
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string(&catalog).map_err(|e| e.to_string())?
                        );
                    } else {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&catalog).map_err(|e| e.to_string())?
                        );
                    }
                }
                _ => unreachable!("execution commands are handled above"),
            }
            Ok(())
        }
    }
}

fn main() {
    #[cfg(all(feature = "sandbox", target_os = "linux"))]
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("__runner")) {
        let result = ocmini_core::sandbox::runner_main();
        ocmini_tools_stop();
        std::process::exit(if result.is_ok() { 0 } else { 78 });
    }

    let mut command = Cli::command();
    let matches = command
        .try_get_matches_from_mut(std::env::args_os())
        .unwrap_or_else(|error| {
            if error.kind() == clap::error::ErrorKind::DisplayVersion {
                // T3's version probe expects the bare compatibility version.
                println!("{OPENCODE_COMPAT_VERSION}");
                std::process::exit(0);
            }
            error.exit()
        });
    // Only default-TUI options conflict with subcommands. Process-wide options
    // such as --sandbox-preset must work before or after any subcommand.
    if matches.subcommand_name().is_some() && matches.contains_id("tui_options") {
        command
            .error(
                clap::error::ErrorKind::ArgumentConflict,
                "TUI options must follow the 'tui' subcommand when using a subcommand",
            )
            .exit();
    }
    #[cfg(feature = "sandbox")]
    if matches
        .subcommand_name()
        .is_some_and(|name| !matches!(name, "run" | "tui" | "serve"))
        && matches.contains_id("sandbox_options")
    {
        command
            .error(
                clap::error::ErrorKind::ArgumentConflict,
                "Sandbox overrides require the default TUI or a 'run', 'tui', or 'serve' command",
            )
            .exit();
    }
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|error| error.exit());
    if let Some(preset) = &cli.sandbox_preset {
        std::env::set_var("OCMINI_SANDBOX_PRESET", preset);
    }
    // One async worker is enough for provider I/O. Blocking database sections
    // hand the scheduler off through block_in_place instead of stalling health,
    // streaming or cancellation. TOKIO_WORKER_THREADS still overrides the count.
    let mut runtime = tokio::runtime::Builder::new_multi_thread();
    if std::env::var_os("TOKIO_WORKER_THREADS").is_none() {
        runtime.worker_threads(1);
    }
    let result = runtime
        .enable_all()
        .build()
        .map_err(|error| format!("Cannot start runtime: {error}"))
        .and_then(|runtime| runtime.block_on(run(cli)));
    // Background shell jobs outlive their tool call; they end with the runtime.
    tools::stop_background_jobs();
    if let Err(error) = result {
        eprintln!("ocmini: {error}");
        std::process::exit(if error.contains("Sandbox startup refused:") {
            78
        } else {
            1
        });
    }
}

#[cfg(all(feature = "sandbox", target_os = "linux"))]
fn ocmini_tools_stop() {
    tools::stop_background_jobs();
}
