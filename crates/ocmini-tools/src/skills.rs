use super::{failure, resolve_path};
use crate::types::{OperationError, ToolOutput};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct Input {
    pub name: String,
}

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
    // Keep the last occurrence of each root to preserve last-wins precedence
    // without walking overlapping global and ancestor directories twice.
    let mut seen = std::collections::HashSet::new();
    let mut unique = Vec::with_capacity(roots.len());
    for root in roots.into_iter().rev() {
        let key = root.canonicalize().unwrap_or_else(|_| root.clone());
        if seen.insert(key) {
            unique.push(root);
        }
    }
    unique.reverse();
    let mut skills = BTreeMap::new();
    for root in unique {
        for path in discover(&root) {
            if let Some(skill) = read_skill(&path) {
                skills.insert(skill["name"].as_str().unwrap().to_owned(), skill);
            }
        }
    }
    skills.into_values().collect()
}

fn discover(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in walkdir::WalkDir::new(root)
        .follow_links(true)
        .into_iter()
        .filter_entry(|entry| {
            entry.depth() == 0 || !entry.file_name().to_string_lossy().starts_with('.')
        })
    {
        match entry {
            Ok(entry) if entry.file_type().is_file() && entry.file_name() == "SKILL.md" => {
                files.push(entry.into_path())
            }
            Err(error) if root.exists() => eprintln!(
                "ocmini: cannot discover skills in {}: {error}",
                root.display()
            ),
            _ => {}
        }
    }
    files.sort();
    files
}

fn read_skill(path: &Path) -> Option<Value> {
    let raw = std::fs::read_to_string(path).ok()?;
    let (fields, content) = crate::config::markdown_frontmatter(&raw)
        .unwrap_or_else(|_| (Default::default(), raw.as_str()));
    let fallback = path.parent()?.file_name()?.to_str()?;
    let name = fields
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(fallback);
    if name.trim().is_empty() {
        return None;
    }
    let path = path.canonicalize().unwrap_or_else(|_| path.to_owned());
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    raw.hash(&mut hash);
    let fingerprint = format!("{:016x}", hash.finish());
    Some(
        json!({"name":name,"id":name,"description":fields.get("description").and_then(Value::as_str).unwrap_or_default().trim(),"location":path,"content":content.trim(),"sourceFingerprint":fingerprint}),
    )
}

#[cfg(test)]
pub fn load(directory: &str, extra: &[String], name: &str) -> Result<ToolOutput, OperationError> {
    let mut out = load_content(directory, extra, name)?;
    render_content(&mut out);
    Ok(out)
}

pub fn load_content(
    directory: &str,
    extra: &[String],
    name: &str,
) -> Result<ToolOutput, OperationError> {
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
    let _ = std::fs::read_dir(base)
        .map_err(|error| failure(format!("Cannot read skill resources: {error}")))?;
    let mut resources = Vec::new();
    for entry in walkdir::WalkDir::new(base)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|entry| {
            entry.depth() == 0 || !entry.file_name().to_string_lossy().starts_with('.')
        })
    {
        let Ok(entry) = entry else {
            continue;
        };
        if entry.depth() == 0 || entry.file_type().is_dir() {
            continue;
        }
        if entry.depth() == 1 && entry.file_name() == "SKILL.md" {
            continue;
        }
        resources.push(entry.path().display().to_string());
    }
    resources.sort();
    resources.truncate(100);
    Ok(ToolOutput {
        title: format!("Loaded skill {name}"),
        output: skill["content"].as_str().unwrap_or_default().into(),
        metadata: json!({"name":name,"location":location,"resources":resources,"sourceFingerprint":skill["sourceFingerprint"]}),
    })
}

pub fn render_content(out: &mut ToolOutput) {
    let name = out.metadata["name"].as_str().unwrap_or_default();
    let location = out.metadata["location"].as_str().unwrap_or_default();
    let base = Path::new(location)
        .parent()
        .unwrap_or_else(|| Path::new(location));
    let resources = out.metadata["resources"]
        .as_array()
        .map(|values| values.iter().filter_map(Value::as_str).collect::<Vec<_>>())
        .unwrap_or_default();
    out.output = format!("<skill_content name={}>\n{}\n\nSkill directory: {}\nRelative resource paths in these instructions are resolved from this directory.\n{}\n</skill_content>",
        serde_json::to_string(name).unwrap(),out.output,base.display(),if resources.is_empty(){String::new()}else{format!("Resources:\n{}",resources.join("\n"))});
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_yaml_quotes_folded_descriptions_and_body() {
        let (fields, body) = crate::config::markdown_frontmatter("---\nname: 'code-review'\ndescription: >-\n  Read and improve\n  code carefully.\n---\n# Instructions\nAct.\n").unwrap();
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
        assert!(result
            .output
            .contains(root.canonicalize().unwrap().to_str().unwrap()));
    }
    #[test]
    fn transformed_body_preserves_source_and_resource_provenance() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("skills/example");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("SKILL.md"),
            "---\nname: provenance-test\n---\noriginal body",
        )
        .unwrap();
        std::fs::write(root.join("resource.txt"), "resource").unwrap();
        let mut out = load_content(
            temporary.path().to_str().unwrap(),
            &["skills".into()],
            "provenance-test",
        )
        .unwrap();
        assert_eq!(out.output, "original body");
        let metadata = out.metadata.clone();
        out.output = "transformed body".into();
        render_content(&mut out);
        assert!(out.output.contains("transformed body"));
        assert!(!out.output.contains("original body"));
        assert!(out
            .output
            .contains(root.canonicalize().unwrap().to_str().unwrap()));
        assert!(out.output.contains("resource.txt"));
        assert_eq!(out.metadata, metadata);
        assert!(!out.metadata["sourceFingerprint"]
            .as_str()
            .unwrap()
            .is_empty());
    }
    #[test]
    fn discovery_does_not_silently_hide_deep_skills() {
        let root = tempfile::tempdir().unwrap();
        let mut path = root.path().to_owned();
        for _ in 0..14 {
            path.push("nested");
        }
        std::fs::create_dir_all(&path).unwrap();
        let skill = path.join("SKILL.md");
        std::fs::write(&skill, "---\nname: deep-skill\n---\nInstructions").unwrap();
        assert!(discover(root.path()).contains(&skill));
    }

    #[test]
    fn skill_resources_are_discovered_recursively_in_sorted_order() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("skills/nested-res");
        std::fs::create_dir_all(root.join("scripts/sub")).unwrap();
        std::fs::create_dir_all(root.join("references")).unwrap();
        std::fs::create_dir_all(root.join(".hidden")).unwrap();
        std::fs::write(
            root.join("SKILL.md"),
            "---\nname: nested-res\n---\nInstructions",
        )
        .unwrap();
        std::fs::write(root.join("z_top.txt"), "top").unwrap();
        std::fs::write(root.join("scripts/b.sh"), "b").unwrap();
        std::fs::write(root.join("scripts/a.sh"), "a").unwrap();
        std::fs::write(root.join("scripts/sub/deep.py"), "deep").unwrap();
        std::fs::write(root.join("references/guide.md"), "guide").unwrap();
        std::fs::write(root.join(".hidden/secret.txt"), "hidden").unwrap();

        let out = load(
            temporary.path().to_str().unwrap(),
            &["skills".into()],
            "nested-res",
        )
        .unwrap();
        let base = root.canonicalize().unwrap();
        let resources: Vec<&str> = out.metadata["resources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        let expected: Vec<String> = [
            "references/guide.md",
            "scripts/a.sh",
            "scripts/b.sh",
            "scripts/sub/deep.py",
            "z_top.txt",
        ]
        .iter()
        .map(|rel| base.join(rel).display().to_string())
        .collect();
        assert_eq!(resources, expected);
        assert!(out.output.contains("scripts/sub/deep.py"));
        assert!(!out.output.contains("secret.txt"));
    }
}
