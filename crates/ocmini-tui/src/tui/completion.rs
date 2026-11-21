use super::{Draft, Key};
use crate::commands::Registry;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
enum Mode {
    Command,
    Arg(String),
    AtFile,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct KeyState {
    mode: Mode,
    prefix: String,
    directory: Option<PathBuf>,
}

pub(super) struct CompletionContext<'a> {
    pub config: &'a crate::config::Config,
    pub auth: &'a crate::auth::AuthStore,
    pub directory: &'a Path,
    pub active_model: Option<&'a crate::types::ModelRef>,
    pub sandbox_presets: &'a [String],
}

#[derive(Default)]
pub(super) struct Completion {
    catalog: Option<Vec<(String, String)>>,
    state: Option<KeyState>,
    entries: Vec<(String, String)>,
    selected: usize,
    dismissed: bool,
}

impl Completion {
    #[cfg(test)]
    pub fn update(&mut self, draft: &Draft, registry: Registry<'_>) {
        self.update_commands_only(draft, registry);
    }

    pub fn update_with(&mut self, draft: &Draft, ctx: &CompletionContext<'_>) {
        let registry = Registry::new(ctx.config);
        if let Some(prefix) = draft.command_prefix() {
            self.update_command(prefix, registry);
            return;
        }
        if let Some((cmd, prefix)) = draft.command_argument_prefix() {
            if matches!(cmd, "model" | "agent" | "variant" | "sandbox" | "attach") {
                let next = KeyState {
                    mode: Mode::Arg(cmd.to_owned()),
                    prefix: prefix.to_owned(),
                    directory: (cmd == "attach").then(|| ctx.directory.to_path_buf()),
                };
                if self.state.as_ref() == Some(&next) {
                    return;
                }
                self.state = Some(next);
                self.selected = 0;
                self.dismissed = false;
                self.entries = argument_candidates(cmd, prefix, ctx);
                return;
            }
        }
        if let Some((_, _, prefix)) = draft.at_file_token() {
            let next = KeyState {
                mode: Mode::AtFile,
                prefix: prefix.to_owned(),
                directory: Some(ctx.directory.to_path_buf()),
            };
            if self.state.as_ref() == Some(&next) {
                return;
            }
            self.state = Some(next);
            self.selected = 0;
            self.dismissed = false;
            self.entries = workspace_files(ctx.directory, prefix, 40);
            return;
        }
        self.clear();
    }

    #[cfg(test)]
    fn update_commands_only(&mut self, draft: &Draft, registry: Registry<'_>) {
        if let Some(prefix) = draft.command_prefix() {
            self.update_command(prefix, registry);
        } else {
            self.clear();
        }
    }

    fn update_command(&mut self, prefix: &str, registry: Registry<'_>) {
        let next = KeyState {
            mode: Mode::Command,
            prefix: prefix.to_owned(),
            directory: None,
        };
        if self.state.as_ref() == Some(&next) {
            return;
        }
        self.state = Some(next);
        self.selected = 0;
        self.dismissed = false;
        let catalog = self.catalog.get_or_insert_with(|| {
            let mut commands: Vec<_> = registry
                .terminal()
                .map(|command| (command.name().to_owned(), command.description().to_owned()))
                .collect();
            commands.sort_unstable_by(|left, right| left.0.cmp(&right.0));
            commands
        });
        let start = catalog.partition_point(|(name, _)| name.as_str() < prefix);
        let count = catalog[start..].partition_point(|(name, _)| name.starts_with(prefix));
        self.entries = catalog[start..start + count].to_vec();
    }

    fn clear(&mut self) {
        self.state = None;
        self.entries.clear();
        self.selected = 0;
        self.dismissed = false;
    }

    fn active_entries(&self) -> &[(String, String)] {
        if self.dismissed {
            return &[];
        }
        &self.entries
    }

    pub fn display_prefix(&self) -> &'static str {
        match self.state.as_ref().map(|s| &s.mode) {
            Some(Mode::Command) => "/",
            Some(Mode::AtFile) => "@",
            Some(Mode::Arg(_)) | None => "",
        }
    }

    pub fn visible(&self, rows: usize) -> impl Iterator<Item = (&str, &str, bool)> {
        let first = self.selected.saturating_sub(rows.saturating_sub(1));
        self.active_entries()
            .iter()
            .enumerate()
            .skip(first)
            .take(rows)
            .map(|(index, (name, description))| {
                (name.as_str(), description.as_str(), index == self.selected)
            })
    }

    #[cfg(test)]
    pub fn key(&mut self, key: &Key, draft: &mut Draft) -> bool {
        self.apply_key(key, draft, None).0
    }

    pub fn apply_key(
        &mut self,
        key: &Key,
        draft: &mut Draft,
        root: Option<&Path>,
    ) -> (bool, Option<String>) {
        let entries = self.active_entries();
        if entries.is_empty() {
            return (false, None);
        }
        match key {
            Key::Up(n) => self.selected = self.selected.saturating_sub(*n),
            Key::Down(n) => self.selected = (self.selected + *n).min(entries.len() - 1),
            Key::Tab => {
                let picked = entries[self.selected].0.clone();
                match self.state.as_ref().map(|s| &s.mode) {
                    Some(Mode::Command) => draft.complete_command(&picked),
                    Some(Mode::Arg(_)) => draft.complete_argument(&picked),
                    Some(Mode::AtFile) => {
                        draft.complete_at_file(&picked);
                        if let Some(dir) = root {
                            match crate::attachments::from_file(&dir.join(&picked)) {
                                Ok(attachment) => {
                                    if !draft
                                        .attachments
                                        .iter()
                                        .any(|existing| existing.url == attachment.url)
                                    {
                                        draft.attachments.push(attachment);
                                    }
                                }
                                Err(err) => return (true, Some(err)),
                            }
                        }
                    }
                    None => {}
                }
            }
            Key::Esc => self.dismissed = true,
            _ => return (false, None),
        }
        (true, None)
    }
}

fn argument_candidates(
    cmd: &str,
    prefix: &str,
    ctx: &CompletionContext<'_>,
) -> Vec<(String, String)> {
    let lower = prefix.to_lowercase();
    match cmd {
        "model" => {
            let connected = ctx.auth.connected_for(ctx.config);
            let mut items: Vec<(String, String)> = ctx
                .config
                .catalog()
                .into_iter()
                .filter(|m| {
                    connected.is_empty()
                        || connected
                            .iter()
                            .any(|p| m["providerID"].as_str() == Some(p.as_str()))
                })
                .filter_map(|m| {
                    let provider = m["providerID"].as_str()?;
                    let model_id = m["modelID"].as_str()?;
                    let full = format!("{provider}/{model_id}");
                    let name = m["name"].as_str().unwrap_or(model_id).to_owned();
                    full.to_lowercase().contains(&lower).then_some((full, name))
                })
                .collect();
            items.sort_unstable_by(|a, b| a.0.cmp(&b.0));
            items
        }
        "agent" => {
            let mut items: Vec<(String, String)> = crate::inventory::agents_json(ctx.config)
                .as_array()
                .into_iter()
                .flatten()
                .filter(|a| a["hidden"] != true)
                .filter_map(|a| {
                    let name = a["name"].as_str()?;
                    let desc = a["description"].as_str().unwrap_or_default().to_owned();
                    name.to_lowercase()
                        .starts_with(&lower)
                        .then_some((name.to_owned(), desc))
                })
                .collect();
            items.sort_unstable_by(|a, b| a.0.cmp(&b.0));
            items
        }
        "variant" => {
            let mut items = vec![("default".to_owned(), "Clear explicit variant".to_owned())];
            let model_ref = ctx
                .active_model
                .cloned()
                .or_else(|| crate::types::ModelRef::parse(&ctx.config.model).ok());
            if let Some(mref) = model_ref {
                if let Some(model) = ctx.config.catalog().into_iter().find(|m| {
                    m["providerID"].as_str() == Some(mref.provider_id.as_str())
                        && m["modelID"].as_str() == Some(mref.model_id.as_str())
                }) {
                    if let Some(variants) = model["variants"].as_object() {
                        for key in variants.keys() {
                            items.push((key.clone(), String::new()));
                        }
                    }
                }
            }
            items.retain(|(name, _)| name.to_lowercase().starts_with(&lower));
            items.sort_unstable_by(|a, b| a.0.cmp(&b.0));
            items
        }
        "sandbox" => {
            let mut items: Vec<(String, String)> = ctx
                .sandbox_presets
                .iter()
                .filter(|p| p.to_lowercase().starts_with(&lower))
                .map(|p| (p.clone(), "Sandbox preset".to_owned()))
                .collect();
            items.sort_unstable_by(|a, b| a.0.cmp(&b.0));
            items
        }
        "attach" => workspace_files(ctx.directory, prefix, 40),
        _ => Vec::new(),
    }
}

/// Walk `root` for relative file paths matching `query`.
pub(super) fn workspace_files(root: &Path, query: &str, limit: usize) -> Vec<(String, String)> {
    let lower = query.to_lowercase();
    let include_hidden = query.starts_with('.') || query.contains("/.");
    let mut stack = vec![root.to_path_buf()];
    let mut matches: Vec<(u8, String)> = Vec::new();
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if matches!(name_str.as_ref(), ".git" | "target" | "node_modules") {
                continue;
            }
            if !include_hidden && name_str.starts_with('.') {
                continue;
            }
            let path = entry.path();
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if ft.is_dir() {
                stack.push(path);
                continue;
            }
            if !ft.is_file() {
                continue;
            }
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            let rel_lower = rel_str.to_lowercase();
            if !lower.is_empty() && !rel_lower.contains(&lower) {
                continue;
            }
            let rank = if rel_lower.starts_with(&lower) {
                0
            } else if name_str.to_lowercase().starts_with(&lower) {
                1
            } else {
                2
            };
            matches.push((rank, rel_str));
        }
    }
    matches.sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    matches.truncate(limit);
    matches
        .into_iter()
        .map(|(_, path)| (path, String::new()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthStore;
    use crate::config::Config;
    use serde_json::json;

    #[test]
    fn alternatives_are_selectable_and_enter_still_submits() {
        let config: Config = serde_json::from_value(json!({
            "command": {"compare": {"template": "Compare $ARGUMENTS", "description": "Compare files"}}
        }))
        .unwrap();
        let registry = || Registry::new(&config);
        let mut completion = Completion::default();
        let mut draft = Draft::default();
        draft.insert_str("/co");
        completion.update(&draft, registry());
        assert_eq!(
            completion
                .visible(3)
                .map(|entry| entry.0)
                .collect::<Vec<_>>(),
            ["compact", "compare", "context"]
        );
        assert!(!completion.key(&Key::Enter, &mut draft));
        assert!(completion.key(&Key::Down(1), &mut draft));
        assert_eq!(
            completion.visible(1).next(),
            Some(("compare", "Compare files", true))
        );
        assert!(completion.key(&Key::Tab, &mut draft));
        assert_eq!(draft.text, "/compare ");
        completion.update(&draft, registry());
        assert!(!completion.key(&Key::Up(1), &mut draft));
        assert!(completion.visible(3).next().is_none());
    }

    #[test]
    fn completion_preserves_arguments_and_is_undoable() {
        let mut config = Config::default();
        config.command.insert("écho".into(), json!("$ARGUMENTS"));
        let mut completion = Completion::default();
        let mut draft = Draft::default();
        draft.insert_str(" /étypo\u{2003}\"two words\"\nmore");
        draft.cursor = 3;
        completion.update(&draft, Registry::new(&config));
        assert!(completion.key(&Key::Tab, &mut draft));
        assert_eq!(draft.text, " /écho\u{2003}\"two words\"\nmore");
        assert_eq!(draft.cursor, 7);
        draft.undo();
        assert_eq!(draft.text, " /étypo\u{2003}\"two words\"\nmore");
        assert_eq!(draft.cursor, 3);
    }

    #[test]
    fn escape_dismisses_until_the_prefix_changes() {
        let config = Config::default();
        let mut completion = Completion::default();
        let mut draft = Draft::default();
        draft.insert('/');
        completion.update(&draft, Registry::new(&config));
        assert!(completion.key(&Key::Esc, &mut draft));
        completion.update(&draft, Registry::new(&config));
        assert!(!completion.key(&Key::Tab, &mut draft));
        draft.insert('h');
        completion.update(&draft, Registry::new(&config));
        assert_eq!(completion.visible(1).next().unwrap().0, "help");
    }

    #[test]
    fn argument_and_at_file_completion_attach_supported_files() {
        let dir = crate::test_tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "pub fn hi() {}\n").unwrap();
        let config: Config = serde_json::from_value(json!({
            "model": "mock/alpha",
            "provider": {
                "mock": {
                    "npm": "@ai-sdk/openai-compatible",
                    "options": {"baseURL": "http://127.0.0.1:9/v1"},
                    "models": {
                        "alpha": {"name": "Alpha", "variants": {"high": {}, "low": {}}},
                        "beta": {"name": "Beta"}
                    }
                }
            }
        }))
        .unwrap();
        let auth = AuthStore::from_json("{}").unwrap();
        let presets = vec!["off".into(), "read-only".into(), "workspace-write".into()];
        let ctx = CompletionContext {
            config: &config,
            auth: &auth,
            directory: dir.path(),
            active_model: None,
            sandbox_presets: &presets,
        };

        // /model argument completion
        let mut completion = Completion::default();
        let mut draft = Draft::default();
        draft.insert_str("/model mock/a");
        completion.update_with(&draft, &ctx);
        assert_eq!(completion.display_prefix(), "");
        assert_eq!(completion.visible(5).next().unwrap().0, "mock/alpha");
        assert!(
            completion
                .apply_key(&Key::Tab, &mut draft, Some(dir.path()))
                .0
        );
        assert_eq!(draft.text, "/model mock/alpha");

        // /sandbox argument completion
        let mut draft = Draft::default();
        draft.insert_str("/sandbox work");
        completion.update_with(&draft, &ctx);
        assert_eq!(completion.visible(5).next().unwrap().0, "workspace-write");
        assert!(
            completion
                .apply_key(&Key::Tab, &mut draft, Some(dir.path()))
                .0
        );
        assert_eq!(draft.text, "/sandbox workspace-write");

        // /variant argument completion
        let mut draft = Draft::default();
        draft.insert_str("/variant h");
        completion.update_with(&draft, &ctx);
        assert_eq!(completion.visible(5).next().unwrap().0, "high");

        // @file completion attaches non-image file
        let mut draft = Draft::default();
        draft.insert_str("Review @src/li");
        completion.update_with(&draft, &ctx);
        assert_eq!(completion.display_prefix(), "@");
        assert_eq!(completion.visible(5).next().unwrap().0, "src/lib.rs");
        let (handled, notice) = completion.apply_key(&Key::Tab, &mut draft, Some(dir.path()));
        assert!(handled);
        assert!(notice.is_none());
        assert_eq!(draft.text, "Review @src/lib.rs ");
        assert_eq!(draft.attachments.len(), 1);
        assert_eq!(draft.attachments[0].filename, "lib.rs");
        assert_eq!(draft.attachments[0].mime, "text/plain");
    }
}
