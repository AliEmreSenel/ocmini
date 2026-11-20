//! Cursor paging over the existing session index; cursors retain their filters.
use crate::types::OperationError;
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Filter {
    pub ascending: bool,
    pub directory: Option<String>,
    pub project: Option<String>,
    pub search: Option<String>,
    pub parent: Option<String>,
    pub archived: Option<bool>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Cursor {
    pub filter: Filter,
    pub time: i64,
    pub id: String,
    pub previous: bool,
}

/// SQLite LIKE only folds ASCII. Expand single-character case forms into GLOB
/// classes while preserving LIKE's wildcards and treating GLOB syntax literally.
pub(crate) fn title_pattern(search: &str) -> String {
    let mut pattern = String::from("*");
    for ch in search.chars() {
        match ch {
            '%' => pattern.push('*'),
            '_' => pattern.push('?'),
            '[' => pattern.push_str("[[]"),
            '*' => pattern.push_str("[*]"),
            '?' => pattern.push_str("[?]"),
            _ => {
                let mut lower = ch.to_lowercase();
                let mut upper = ch.to_uppercase();
                let lower = lower.next().filter(|form| lower.len() == 0 && *form != ch);
                let upper = upper.next().filter(|form| upper.len() == 0 && *form != ch);
                if lower.is_some() || upper.is_some() {
                    pattern.push('[');
                    pattern.push(ch);
                    pattern.extend(lower);
                    pattern.extend(upper.filter(|form| Some(*form) != lower));
                    pattern.push(']');
                } else {
                    pattern.push(ch);
                }
            }
        }
    }
    pattern.push('*');
    pattern
}

impl Cursor {
    pub fn decode(value: &str) -> Result<Self, OperationError> {
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(value)
            .map_err(|_| OperationError::invalid("Invalid session cursor"))?;
        {
            let cursor: Self = serde_json::from_slice(&bytes)
                .map_err(|_| OperationError::invalid("Invalid session cursor"))?;
            if cursor
                .filter
                .directory
                .as_ref()
                .is_some_and(|d| !std::path::Path::new(d).is_absolute())
            {
                return Err(OperationError::invalid("Invalid session cursor"));
            }
            Ok(cursor)
        }
    }
    pub fn encode(&self) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(self).expect("session cursor JSON"))
    }
}
pub fn filter(query: &HashMap<String, String>) -> Result<Filter, OperationError> {
    let order = query.get("order").map(String::as_str).unwrap_or("desc");
    if !matches!(order, "asc" | "desc") {
        return Err(OperationError::invalid("order must be asc or desc"));
    }
    let directory = query
        .get("directory")
        .map(|dir| {
            let path = std::path::Path::new(dir);
            if !path.is_absolute() {
                return Err(OperationError::invalid("directory must be absolute"));
            }
            Ok(crate::config::directory_identity(path)
                .to_string_lossy()
                .into_owned())
        })
        .transpose()?;
    if query
        .get("parentID")
        .is_some_and(|id| id != "null" && !id.starts_with("ses_"))
    {
        return Err(OperationError::invalid("Invalid parentID"));
    }
    if query.contains_key("subpath") {
        return Err(OperationError::invalid(
            "Project-relative subpath filtering is not supported; use directory",
        ));
    }
    let archived = query
        .get("archived")
        .map(|v| match v.as_str() {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err(OperationError::invalid("archived must be true or false")),
        })
        .transpose()?;
    Ok(Filter {
        ascending: order == "asc",
        directory,
        project: query.get("project").cloned(),
        search: query.get("search").cloned(),
        parent: query.get("parentID").cloned(),
        archived,
    })
}
