use serde_json::{Map, Value};
pub fn markdown_frontmatter(content: &str) -> Result<(Map<String, Value>, &str), String> {
    let content = content.trim_start_matches('\u{feff}');
    let Some(rest) = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))
    else {
        return Ok((Map::new(), content));
    };
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            let frontmatter: Value =
                yaml_serde::from_str(&rest[..offset]).map_err(|e| e.to_string())?;
            let map = if frontmatter.is_null() {
                Map::new()
            } else {
                frontmatter
                    .as_object()
                    .cloned()
                    .ok_or("YAML frontmatter must be a mapping")?
            };
            return Ok((map, &rest[offset + line.len()..]));
        }
        offset += line.len();
    }
    Err("missing closing --- delimiter".into())
}
