use super::*;

pub(super) async fn system_prompt(
    cfg: &Config,
    directory: &str,
    agent: &str,
    provided: Option<&str>,
) -> Result<String, String> {
    let mut system = system_prompt_base(cfg, directory, agent).await?;
    if let Some(provided) = provided {
        system.push_str("\n\n");
        system.push_str(provided);
    }
    Ok(system)
}

/// The AGENTS.md/skills/agent-prompt base is identical for every turn in a
/// directory; reading those files per turn scales with sessions, so cache it.
async fn system_prompt_base(cfg: &Config, directory: &str, agent: &str) -> Result<String, String> {
    type PromptCache = Mutex<HashMap<(String, String), (std::time::Instant, String)>>;
    static CACHE: std::sync::OnceLock<PromptCache> = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let key = (directory.to_string(), agent.to_string());
    if let Some((created, prompt)) = cache.lock().unwrap().get(&key) {
        if created.elapsed() < std::time::Duration::from_secs(10) {
            return Ok(prompt.clone());
        }
    }
    let mut system=format!("You are ocmini, a coding agent. Work in {directory}. Read the code before changing it, follow project instructions, and report real tool results. Use available tools to complete the user's task. Operating system: {}.",std::env::consts::OS);
    let mut files = crate::blocking::run(|| {
        let mut files = Vec::new();
        let global = PathBuf::from(crate::config::config_home()).join("opencode/AGENTS.md");
        if global.is_file() {
            files.push(global);
        }
        let mut ancestors = Vec::new();
        for parent in Path::new(directory).ancestors() {
            ancestors.push(parent.to_path_buf());
            if parent.join(".git").exists() {
                break;
            }
        }
        ancestors.reverse();
        for base in ancestors {
            if base.join("AGENTS.md").is_file() {
                files.push(base.join("AGENTS.md"));
            }
        }
        files
    });
    let mut seen = HashSet::new();
    for instruction in &cfg.instructions {
        if instruction.starts_with("https://") || instruction.starts_with("http://") {
            let response = provider::client()
                .get(instruction)
                .timeout(std::time::Duration::from_secs(15))
                .send()
                .await
                .map_err(|e| format!("Instruction URL {instruction}: {e}"))?
                .error_for_status()
                .map_err(|e| e.to_string())?;
            let bytes = crate::http::read_body(response, None, 1024 * 1024).await?;
            system.push_str(&format!(
                "\n\n# Instructions from {instruction}\n{}",
                String::from_utf8(bytes).map_err(|_| "Instructions are not UTF-8")?
            ));
            continue;
        }
        crate::blocking::run(|| -> Result<(), String> {
            let path = tools::resolve_path(directory, instruction);
            let pattern = path.to_string_lossy();
            for matched in
                glob::glob(&pattern).map_err(|e| format!("Instruction path {instruction}: {e}"))?
            {
                let path = matched.map_err(|e| e.to_string())?;
                if path.is_file() {
                    files.push(path);
                }
            }
            Ok(())
        })?;
    }
    let system = crate::blocking::run(|| -> Result<String, String> {
        for path in files {
            let canonical = std::fs::canonicalize(&path).map_err(|e| e.to_string())?;
            if !seen.insert(canonical) {
                continue;
            }
            let contents = std::fs::read_to_string(&path)
                .map_err(|e| format!("Cannot read instructions {}: {e}", path.display()))?;
            system.push_str(&format!(
                "\n\n# Instructions from {}\n{}",
                path.display(),
                contents
            ));
        }
        if let Some(prompt) = cfg.agent.get(agent).and_then(|a| a["prompt"].as_str()) {
            system.push_str("\n\n");
            system.push_str(prompt);
        }
        let skills = tools::skill_list_with_paths(Some(directory), &cfg.skill_paths());
        if !skills.is_empty() {
            system.push_str(
                "\n\nAvailable skills (load their instructions with the skill tool when relevant):",
            );
            for skill in skills {
                system.push_str(&format!(
                    "\n- {}: {}",
                    skill["name"].as_str().unwrap_or_default(),
                    skill["description"].as_str().unwrap_or_default()
                ));
            }
        }
        Ok(system)
    })?;
    let mut cache = cache.lock().unwrap();
    cache.retain(|_, (created, _)| created.elapsed() < std::time::Duration::from_secs(10));
    cache.insert(key, (std::time::Instant::now(), system.clone()));
    Ok(system)
}
