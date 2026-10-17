mod auth;
mod bus;
mod config;
mod engine;
mod engine_impl;
mod mcp;
mod permission;
mod provider;
mod server;
mod store;
mod tools;
mod tui;
mod types;
mod usage_db;

pub const OPENCODE_COMPAT_VERSION: &str = "1.19.2-ocmini.0.2.0";

const HELP: &str = "ocmini — a compact OpenCode-compatible coding runtime

Usage:
  ocmini [tui] [--model provider/model] [--directory DIR]
                                           Start the interactive terminal chat
  ocmini serve [--hostname HOST] [--port N] Start the HTTP/SSE server
  ocmini models [PROVIDER] [--verbose]      List available models
  ocmini agent list                       List available agents
  ocmini debug skill                      Print discovered skills as JSON
  ocmini --version                        Print the compatibility version

serve defaults to 127.0.0.1:4096. Port 0 selects an available port.
Both --option=value and --option value are accepted.
Set OPENCODE_SERVER_PASSWORD to require HTTP Basic authentication.
OCMINI_MAX_REQUEST_BYTES sets the JSON body limit (default 128 MiB).

The 1.19.2-ocmini version identifies the OpenCode 1.x wire contract.
ocmini is an independent implementation; it does not implement OpenCode 2.x /api.
";

fn run_tui(args: &[String]) -> Result<(), String> {
    let mut model = None;
    let mut directory = None;
    let mut index = 0;
    while index < args.len() {
        let (option, inline) = args[index]
            .split_once('=')
            .map(|(name, value)| (name, Some(value)))
            .unwrap_or((args[index].as_str(), None));
        let value = match inline {
            Some(value) => value,
            None => {
                index += 1;
                args.get(index).map(String::as_str).ok_or_else(|| format!("{option} requires a value"))?
            }
        };
        match option {
            "--model" => model = Some(value.to_owned()),
            "--directory" => directory = Some(value.to_owned()),
            "--help" | "-h" => { println!("{HELP}"); return Ok(()); }
            other => return Err(format!("Unknown option: {other}")),
        }
        index += 1;
    }
    let runtime = tokio::runtime::Runtime::new()
        .map_err(|error| format!("Cannot start runtime: {error}"))?;
    runtime.block_on(tui::run(model, directory))
}

fn run(args: Vec<String>) -> Result<(), String> {
    if args.is_empty() {
        return run_tui(&[]);
    }
    match args[0].as_str() {
        "--help" | "-h" | "help" => {
            println!("{HELP}");
            Ok(())
        }
        "--version" | "-V" | "version" if args.len() == 1 => {
            println!("{OPENCODE_COMPAT_VERSION}");
            Ok(())
        }
        "tui" => run_tui(&args[1..]),
        "serve" => {
            let mut hostname = "127.0.0.1".to_owned();
            let mut port = 4096;
            let mut index = 1;
            while index < args.len() {
                if args[index] == "--help" || args[index] == "-h" {
                    println!("{HELP}");
                    return Ok(());
                }
                let (option, inline) = args[index]
                    .split_once('=')
                    .map(|(name, value)| (name, Some(value)))
                    .unwrap_or((args[index].as_str(), None));
                if option != "--hostname" && option != "--port" {
                    return Err(format!("Unknown serve option: {}", args[index]));
                }
                let value = match inline {
                    Some(value) => value,
                    None => {
                        index += 1;
                        args.get(index)
                            .map(String::as_str)
                            .ok_or_else(|| format!("{option} requires a value"))?
                    }
                };
                if option == "--hostname" {
                    if value.is_empty() {
                        return Err("--hostname must not be empty".into());
                    }
                    hostname = value.trim_start_matches('[').trim_end_matches(']').to_owned();
                } else {
                    port = value.parse::<u16>()
                        .map_err(|_| "--port must be an integer between 0 and 65535".to_owned())?;
                }
                index += 1;
            }
            let runtime = tokio::runtime::Runtime::new()
                .map_err(|error| format!("Cannot start runtime: {error}"))?;
            runtime.block_on(server::serve(hostname, port))
        }
        "models" => {
            let mut verbose = false;
            let mut provider = None;
            for argument in &args[1..] {
                match argument.as_str() {
                    "--verbose" => verbose = true,
                    "--help" | "-h" => {
                        println!("Usage: ocmini models [PROVIDER] [--verbose]");
                        return Ok(());
                    }
                    value if value.starts_with('-') => return Err(format!("Unknown models option: {value}")),
                    value if provider.is_none() => provider = Some(value),
                    value => return Err(format!("Unexpected models argument: {value}")),
                }
            }
            let directory = std::env::current_dir().map_err(|error| error.to_string())?;
            let config = config::Config::load_for(&directory.to_string_lossy())?;
            let auth = auth::AuthStore::load();
            let runtime = tokio::runtime::Runtime::new().map_err(|error| error.to_string())?;
            let (config, errors) = runtime.block_on(provider::discover_models(&config, &auth));
            if !errors.is_empty() && config.catalog().is_empty() { return Err(errors.join("; ")); }
            for error in errors { eprintln!("ocmini: model discovery: {error}"); }
            let inventory = server::provider_inventory(&config, &auth);
            let connected = inventory["connected"].as_array().cloned().unwrap_or_default();
            for item in inventory["all"].as_array().into_iter().flatten() {
                let id = item["id"].as_str().unwrap_or_default();
                if provider.is_some_and(|filter| filter != id)
                    || !connected.iter().any(|value| value.as_str() == Some(id)) {
                    continue;
                }
                if let Some(models) = item["models"].as_object() {
                    for (model_id, model) in models {
                        println!("{id}/{model_id}");
                        if verbose {
                            println!("{}", serde_json::to_string(model).map_err(|error| error.to_string())?);
                        }
                    }
                }
            }
            Ok(())
        }
        "agent" if args.len() == 2 && args[1] == "list" => {
            let directory = std::env::current_dir().map_err(|error| error.to_string())?;
            let config = config::Config::load_for(&directory.to_string_lossy())?;
            let agents = server::agents_json(&config);
            for agent in agents.as_array().into_iter().flatten() {
                println!("{} ({})", agent["name"].as_str().unwrap_or_default(), agent["mode"].as_str().unwrap_or("all"));
                println!("{}", agent["permission"]);
            }
            Ok(())
        }
        "debug" if args.len() == 2 && args[1] == "skill" => {
            let directory = std::env::current_dir().map_err(|error| error.to_string())?;
            let config = config::Config::load_for(&directory.to_string_lossy())?;
            println!("{}", server::skills_json_for(&directory.to_string_lossy(), &config));
            Ok(())
        }
        command => Err(format!("Unknown command or arguments: {command}. Run ocmini --help for usage.")),
    }
}

fn main() {
    if let Err(error) = run(std::env::args().skip(1).collect()) {
        eprintln!("ocmini: {error}");
        std::process::exit(1);
    }
}
