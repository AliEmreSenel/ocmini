use super::{failure, resolve_path};
use crate::types::{ToolError, ToolOutput};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

pub fn list(directory: Option<&str>, extra: &[String]) -> Vec<Value> {
    let home = std::env::var("HOME").unwrap_or_default();
    let config = std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| format!("{home}/.config"));
    let mut roots = vec![
        PathBuf::from(&home).join(".agents/skills"),
        PathBuf::from(&home).join(".claude/skills"),
        PathBuf::from(&config).join("opencode/skill"),
        PathBuf::from(&config).join("opencode/skills"),
    ];
    if let Some(directory) = directory {
        let mut ancestors: Vec<_> = Path::new(directory).ancestors().collect();
        ancestors.reverse();
        for ancestor in ancestors {
            roots.push(ancestor.join(".agents/skills"));
            roots.push(ancestor.join(".claude/skills"));
            roots.push(ancestor.join(".opencode/skill"));
            roots.push(ancestor.join(".opencode/skills"));
        }
    }
    for path in extra {
        roots.push(resolve_path(directory.unwrap_or("."), path));
    }
    let mut skills = BTreeMap::new();
    for root in roots {
        for path in discover(&root) {
            if let Some(skill) = read_skill(&path) {
                skills.insert(skill["name"].as_str().unwrap().to_owned(), skill);
            }
        }
    }
    skills.into_values().collect()
}

fn discover(root: &Path) -> Vec<PathBuf> {
    if root.is_file() {
        return if root.file_name().is_some_and(|name| name == "SKILL.md") {
            vec![root.to_owned()]
        } else {
            Vec::new()
        };
    }
    let mut stack = vec![(root.to_owned(), 0)];
    let mut seen = HashSet::new();
    let mut files = Vec::new();
    while let Some((path, depth)) = stack.pop() {
        let Ok(canonical) = path.canonicalize() else {
            continue;
        };
        if !seen.insert(canonical) || depth > 12 || files.len() >= 2000 {
            continue;
        }
        let skill = path.join("SKILL.md");
        if skill.is_file() {
            files.push(skill);
        }
        let Ok(entries) = std::fs::read_dir(path) else {
            continue;
        };
        for entry in entries.flatten() {
            if entry.path().is_dir() && !entry.file_name().to_string_lossy().starts_with('.') {
                stack.push((entry.path(), depth + 1));
            }
        }
    }
    files.sort();
    files
}

fn read_skill(path: &Path) -> Option<Value> {
    let metadata = std::fs::metadata(path).ok()?;
    if metadata.len() > 2 * 1024 * 1024 {
        return None;
    }
    let raw = std::fs::read_to_string(path).ok()?;
    let (fields, content) = frontmatter(&raw);
    let fallback = path.parent()?.file_name()?.to_str()?;
    let name = fields
        .get("name")
        .filter(|s| !s.is_empty())
        .map(String::as_str)
        .unwrap_or(fallback);
    if name.trim().is_empty() {
        return None;
    }
    let path = path.canonicalize().unwrap_or_else(|_| path.to_owned());
    Some(
        json!({"name":name,"id":name,"description":fields.get("description").cloned().unwrap_or_default(),"location":path,"content":content.trim()}),
    )
}

fn frontmatter(raw: &str) -> (BTreeMap<String, String>, &str) {
    let raw = raw.trim_start_matches('\u{feff}');
    let Some(rest) = raw
        .strip_prefix("---\n")
        .or_else(|| raw.strip_prefix("---\r\n"))
    else {
        return (BTreeMap::new(), raw);
    };
    let mut consumed = 0;
    let mut header = String::new();
    let mut found = false;
    for line in rest.split_inclusive('\n') {
        consumed += line.len();
        if line.trim_end_matches(['\r', '\n']) == "---" {
            found = true;
            break;
        }
        header.push_str(line);
    }
    if !found {
        return (BTreeMap::new(), raw);
    }
    let parsed = serde_saphyr::from_str::<Value>(&header).unwrap_or(Value::Null);
    let fields = ["name", "description"]
        .into_iter()
        .filter_map(|key| {
            parsed[key]
                .as_str()
                .map(|value| (key.to_owned(), value.trim().to_owned()))
        })
        .collect();
    (fields, &rest[consumed..])
}

pub(super) fn load(directory: &str, extra: &[String], name: &str) -> Result<ToolOutput, ToolError> {
    let skills = list(Some(directory), extra);
    let skill = skills
        .iter()
        .find(|skill| skill["name"] == name || skill["id"] == name)
        .ok_or_else(|| {
            failure(format!(
                "Unknown skill {name:?}. Available skills: {}",
                skills
                    .iter()
                    .filter_map(|s| s["name"].as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })?;
    let location = skill["location"]
        .as_str()
        .ok_or_else(|| failure("Skill has no location"))?;
    let base = Path::new(location)
        .parent()
        .ok_or_else(|| failure("Skill has no resource directory"))?;
    let resources = std::fs::read_dir(base)
        .map_err(|error| failure(format!("Cannot read skill resources: {error}")))?
        .flatten()
        .filter(|entry| entry.file_name() != "SKILL.md")
        .map(|entry| entry.path().display().to_string())
        .take(100)
        .collect::<Vec<_>>();
    let output = format!("<skill_content name={}>\n{}\n\nSkill directory: {}\nRelative resource paths in these instructions are resolved from this directory.\n{}\n</skill_content>",
        serde_json::to_string(name).unwrap(), skill["content"].as_str().unwrap_or_default(), base.display(), if resources.is_empty() { String::new() } else { format!("Resources:\n{}", resources.join("\n")) });
    Ok(ToolOutput {
        title: format!("Loaded skill {name}"),
        output,
        metadata: json!({"name":name,"location":location,"resources":resources}),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_yaml_quotes_folded_descriptions_and_body() {
        let (fields, body) = frontmatter("---\nname: 'code-review'\ndescription: >-\n  Read and improve\n  code carefully.\n---\n# Instructions\nAct.\n");
        assert_eq!(fields["name"], "code-review");
        assert_eq!(fields["description"], "Read and improve code carefully.");
        assert_eq!(body, "# Instructions\nAct.\n");
    }

    #[test]
    fn configurable_skill_directory_loads_complete_content() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path();
        let root = directory.join("custom/example");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("SKILL.md"),
            "---\nname: unique-test-skill\ndescription: Test\n---\nDo exactly this.\n",
        )
        .unwrap();
        let result = load(
            directory.to_str().unwrap(),
            &["custom".into()],
            "unique-test-skill",
        )
        .unwrap();
        assert!(result.output.contains("Do exactly this."));
        assert!(result.output.contains(root.to_str().unwrap()));
    }
}
