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
    #[cfg_attr(
        feature = "web",
        command(
            visible_alias = "web",
            long_about = "Start the HTTP/SSE server and the browser interface at /"
        )
    )]
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
    /// Inspect configured MCP servers
    Mcp {
        #[command(subcommand)]
        command: McpCommand,
    },
    /// Inspect effective configuration
    Config {
        #[arg(long)]
        json: bool,
        #[arg(long, alias = "dir", value_name = "DIR")]
        directory: Option<String>,
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
enum McpCommand {
    /// List configured MCP servers
    List {
        #[arg(long)]
        json: bool,
        #[arg(long, alias = "dir", value_name = "DIR")]
        directory: Option<String>,
    },
}

#[derive(Subcommand)]
enum SessionCommand {
    /// List saved sessions, newest first
    List {
        #[arg(long, alias = "dir", value_name = "DIR")]
        directory: Option<String>,
        /// Emit JSON array instead of tab-separated lines
        #[arg(long)]
        json: bool,
        /// Limit the number of sessions listed
        #[arg(long)]
        limit: Option<usize>,
        /// Show only top-level (root) sessions
        #[arg(long)]
        roots: bool,
    },
    /// Show a session and its messages
    Show {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Delete a session and its child sessions
    Delete { id: String },
    /// Rename a session
    Rename { id: String, title: String },
    /// Fork a session into a new session
    Fork {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Export a session, its messages, and todos as JSON
    Export { id: String },
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
        Command::Session { command } => match command {
            SessionCommand::List {
                directory,
                json,
                limit,
                roots,
            } => {
                let directory = directory.as_deref().map(sessions::directory).transpose()?;
                let store = store::Store::open(&config::store_path(), &types::hex_id("ocm_"))?;
                let mut sessions = store.list_sessions(directory.as_deref())?;
                if roots {
                    sessions.retain(|s| s.parent_id.is_none());
                }
                if let Some(limit) = limit {
                    sessions.truncate(limit);
                }
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&sessions).map_err(|e| e.to_string())?
                    );
                } else {
                    for session in sessions {
                        println!("{}\t{}\t{}", session.id, session.title, session.directory);
                    }
                }
                Ok(())
            }
            SessionCommand::Show { id, json } => {
                let store = store::Store::open(&config::store_path(), &types::hex_id("ocm_"))?;
                let session = store
                    .get_session(&id)?
                    .ok_or_else(|| format!("Session not found: {id}"))?;
                let messages = store.messages(&id)?;
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "session": session,
                            "messages": messages,
                        }))
                        .map_err(|e| e.to_string())?
                    );
                } else {
                    println!("id:        {}", session.id);
                    println!("title:     {}", session.title);
                    println!("directory: {}", session.directory);
                    if let Some(parent) = &session.parent_id {
                        println!("parent:    {parent}");
                    }
                    let total_tokens = session.tokens.input
                        + session.tokens.output
                        + session.tokens.reasoning
                        + session.tokens.cache.read
                        + session.tokens.cache.write;
                    println!("usage:     {} tokens (${:.4})", total_tokens, session.cost);
                    println!("messages:  {}", messages.len());
                    for message in &messages {
                        let preview = message
                            .parts
                            .iter()
                            .find(|p| p["type"].as_str() == Some("text"))
                            .and_then(|p| p["text"].as_str())
                            .unwrap_or("")
                            .lines()
                            .next()
                            .unwrap_or("");
                        println!("  [{}] {}: {}", message.info.id, message.info.role, preview);
                    }
                }
                Ok(())
            }
            SessionCommand::Delete { id } => {
                with_app(create_app()?, move |app| async move {
                    let session = app
                        .store
                        .get_session(&id)?
                        .ok_or_else(|| format!("Session not found: {id}"))?;
                    sessions::delete(&app, &session).await?;
                    println!("deleted {id}");
                    Ok(())
                })
                .await
            }
            SessionCommand::Rename { id, title } => {
                with_app(create_app()?, move |app| async move {
                    let session = app
                        .store
                        .get_session(&id)?
                        .ok_or_else(|| format!("Session not found: {id}"))?;
                    let updated =
                        sessions::update(&app, &session, serde_json::json!({"title": title}))
                            .await?;
                    println!("{}\t{}", updated.id, updated.title);
                    Ok(())
                })
                .await
            }
            SessionCommand::Fork { id, json } => {
                with_app(create_app()?, move |app| async move {
                    let session = app
                        .store
                        .get_session(&id)?
                        .ok_or_else(|| format!("Session not found: {id}"))?;
                    let forked = sessions::fork(&app, &session, None).await?;
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&forked).map_err(|e| e.to_string())?
                        );
                    } else {
                        println!("{}\t{}\t{}", forked.id, forked.title, forked.directory);
                    }
                    Ok(())
                })
                .await
            }
            SessionCommand::Export { id } => {
                let store = store::Store::open(&config::store_path(), &types::hex_id("ocm_"))?;
                let session = store
                    .get_session(&id)?
                    .ok_or_else(|| format!("Session not found: {id}"))?;
                let messages = store.messages(&id)?;
                let todos = store.backend().todos(&id)?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "session": session,
                        "messages": messages,
                        "todos": todos,
                    }))
                    .map_err(|e| e.to_string())?
                );
                Ok(())
            }
        },
        Command::Mcp {
            command: McpCommand::List { json, directory },
        } => {
            let dir = match directory.as_deref() {
                Some(d) => sessions::directory(d)?,
                None => std::env::current_dir()
                    .map_err(|e| e.to_string())?
                    .to_string_lossy()
                    .into_owned(),
            };
            let cfg = config::Config::load_for(&dir)?;
            let mut entries: Vec<_> = cfg.mcp.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            if json {
                let list: Vec<_> = entries
                    .into_iter()
                    .map(|(name, server)| {
                        serde_json::json!({
                            "name": name,
                            "type": if server.r#type.is_empty() {
                                if server.url.is_empty() { "local" } else { "remote" }
                            } else {
                                server.r#type.as_str()
                            },
                            "enabled": server.enabled.unwrap_or(true),
                            "command": server.command,
                            "url": server.url,
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&list).map_err(|e| e.to_string())?
                );
            } else if entries.is_empty() {
                println!("No MCP servers configured.");
            } else {
                for (name, server) in entries {
                    let kind = if server.r#type.is_empty() {
                        if server.url.is_empty() {
                            "local"
                        } else {
                            "remote"
                        }
                    } else {
                        server.r#type.as_str()
                    };
                    let state = if server.enabled.unwrap_or(true) {
                        "enabled"
                    } else {
                        "disabled"
                    };
                    let target = if !server.url.is_empty() {
                        server.url.clone()
                    } else {
                        server.command.join(" ")
                    };
                    println!("{name}\t{kind}\t{state}\t{target}");
                }
            }
            Ok(())
        }
        Command::Config { json, directory } => {
            let dir = match directory.as_deref() {
                Some(d) => sessions::directory(d)?,
                None => std::env::current_dir()
                    .map_err(|e| e.to_string())?
                    .to_string_lossy()
                    .into_owned(),
            };
            let cfg = config::Config::load_for(&dir)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&cfg).map_err(|e| e.to_string())?
                );
            } else {
                println!("directory:    {dir}");
                println!(
                    "model:        {}",
                    if cfg.model.is_empty() {
                        "(unset)"
                    } else {
                        &cfg.model
                    }
                );
                if !cfg.small_model.is_empty() {
                    println!("small_model:  {}", cfg.small_model);
                }
                if !cfg.title_model.is_empty() {
                    println!("title_model:  {}", cfg.title_model);
                }
                if let Some(sandbox) = cfg.extra.get("sandbox") {
                    println!("sandbox:      {sandbox}");
                }
                println!("scroll_mode:  {:?}", cfg.ui.scroll_mode);
                println!(
                    "compaction:   auto={}, prune={}",
                    cfg.compaction.auto.unwrap_or(true),
                    cfg.compaction.prune.unwrap_or(false)
                );
                let mut providers: Vec<&str> = cfg.provider.keys().map(String::as_str).collect();
                providers.sort_unstable();
                println!(
                    "providers:    {}",
                    if providers.is_empty() {
                        "(none)".into()
                    } else {
                        providers.join(", ")
                    }
                );
                let mut mcp: Vec<&str> = cfg.mcp.keys().map(String::as_str).collect();
                mcp.sort_unstable();
                println!(
                    "mcp:          {}",
                    if mcp.is_empty() {
                        "(none)".into()
                    } else {
                        mcp.join(", ")
                    }
                );
            }
            Ok(())
        }
        Command::Serve { hostname, port, .. } => {
            let app = create_app()?;
            #[cfg(feature = "sandbox")]
            app.sandbox.validate_cli_defaults(&app.config)?;
            #[cfg(feature = "web")]
            let (routes, notice) = (ocmini_web::router(), Some("ocmini web interface"));
            #[cfg(not(feature = "web"))]
            let (routes, notice) = (Default::default(), None);
            with_app(app, |app| {
                server::serve_with(app, hostname, port, routes, notice)
            })
            .await
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
