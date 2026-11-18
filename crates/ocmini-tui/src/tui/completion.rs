use super::{Draft, Key};
use crate::commands::Registry;
use std::ops::Range;

#[derive(Default)]
pub(super) struct Completion {
    catalog: Option<Vec<(String, String)>>,
    prefix: Option<String>,
    matches: Range<usize>,
    selected: usize,
    dismissed: bool,
}

impl Completion {
    pub fn update(&mut self, draft: &Draft, registry: Registry<'_>) {
        let prefix = draft.command_prefix();
        if prefix == self.prefix.as_deref() {
            return;
        }
        self.prefix = prefix.map(str::to_owned);
        self.selected = 0;
        self.dismissed = false;
        self.matches = prefix.map_or(0..0, |prefix| {
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
            start..start + count
        });
    }

    fn entries(&self) -> &[(String, String)] {
        if self.dismissed {
            return &[];
        }
        &self.catalog.as_deref().unwrap_or_default()[self.matches.clone()]
    }

    pub fn visible(&self, rows: usize) -> impl Iterator<Item = (&str, &str, bool)> {
        let first = self.selected.saturating_sub(rows.saturating_sub(1));
        self.entries()
            .iter()
            .enumerate()
            .skip(first)
            .take(rows)
            .map(|(index, (name, description))| {
                (name.as_str(), description.as_str(), index == self.selected)
            })
    }

    pub fn key(&mut self, key: &Key, draft: &mut Draft) -> bool {
        let entries = self.entries();
        if entries.is_empty() {
            return false;
        }
        match key {
            Key::Up(n) => self.selected = self.selected.saturating_sub(*n),
            Key::Down(n) => self.selected = (self.selected + *n).min(entries.len() - 1),
            Key::Tab => draft.complete_command(&entries[self.selected].0),
            Key::Esc => self.dismissed = true,
            _ => return false,
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
