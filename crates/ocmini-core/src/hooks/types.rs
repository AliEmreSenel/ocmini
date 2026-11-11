use crate::events as event;
pub use event::NAMES as EVENT_NAMES;
pub use ocmini_tools::hook_types::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
/// Debug/config inspection assembles declarations; delivery never consults this list.
pub fn declarations() -> impl Iterator<Item = Hook> {
    crate::engine_impl::hooks::HOOKS
        .iter()
        .chain(crate::tools::hooks::HOOKS)
        .chain(crate::provider::hooks::HOOKS)
        .chain(crate::sessions::hooks::HOOKS)
        .chain(crate::permission::HOOKS)
        .copied()
}
#[cfg(test)]
pub fn apply_response(
    name: &str,
    response: Value,
    input: &mut Value,
) -> Result<Option<String>, String> {
    super::test_hook(name).apply(response, input)
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
enum PatternSource {
    One(String),
    Many(Vec<String>),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(try_from = "PatternSource", into = "PatternSource")]
pub struct Patterns {
    source: PatternSource,
    compiled: Vec<glob::Pattern>,
}
impl TryFrom<PatternSource> for Patterns {
    type Error = String;
    fn try_from(source: PatternSource) -> Result<Self, String> {
        let values = match &source {
            PatternSource::One(value) => std::slice::from_ref(value),
            PatternSource::Many(values) => values,
        };
        let compiled = values
            .iter()
            .map(|value| {
                glob::Pattern::new(value)
                    .map_err(|error| format!("invalid matcher {value}: {error}"))
            })
            .collect::<Result<_, _>>()?;
        Ok(Self { source, compiled })
    }
}
impl TryFrom<Vec<String>> for Patterns {
    type Error = String;
    fn try_from(values: Vec<String>) -> Result<Self, String> {
        Self::try_from(PatternSource::Many(values))
    }
}
impl From<Patterns> for PatternSource {
    fn from(patterns: Patterns) -> Self {
        patterns.source
    }
}
impl Patterns {
    pub fn empty() -> Self {
        Self {
            source: PatternSource::Many(Vec::new()),
            compiled: Vec::new(),
        }
    }
    pub fn values(&self) -> impl ExactSizeIterator<Item = &str> {
        let values = match &self.source {
            PatternSource::One(value) => std::slice::from_ref(value),
            PatternSource::Many(values) => values,
        };
        values.iter().map(String::as_str)
    }
    pub fn matches(&self, value: &str) -> bool {
        self.compiled.iter().any(|pattern| pattern.matches(value))
    }
    pub fn validate(&self) -> Result<(), String> {
        if self.compiled.is_empty() {
            Err("a matcher cannot be empty".into())
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Matcher {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directory: Option<Patterns>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<Patterns>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<Patterns>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<Patterns>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_kind: Option<Patterns>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_kind: Option<Patterns>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<Patterns>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Patterns>,
}
impl Matcher {
    pub fn validate(&self) -> Result<(), String> {
        for pattern in [
            &self.directory,
            &self.session,
            &self.agent,
            &self.model,
            &self.turn_kind,
            &self.request_kind,
            &self.tool,
            &self.outcome,
        ]
        .into_iter()
        .flatten()
        {
            pattern.validate()?;
        }
        for (name, patterns, allowed) in [
            ("session", &self.session, &["root", "child"][..]),
            ("turnKind", &self.turn_kind, &["primary", "compaction"][..]),
            (
                "requestKind",
                &self.request_kind,
                &["primary", "compaction", "title"][..],
            ),
        ] {
            if let Some(patterns) = patterns {
                for value in patterns.values() {
                    if !allowed.contains(&value) && value != "*" {
                        return Err(format!("invalid {name} matcher {value}"));
                    }
                }
            }
        }
        Ok(())
    }
    #[cfg(test)]
    pub fn matches(&self, name: &str, context: &Value, defaults: bool) -> bool {
        self.matches_context(
            context,
            if defaults {
                crate::hooks::test_hook(name).defaults
            } else {
                Defaults::default()
            },
        )
    }
    pub fn matches_context(&self, context: &Value, defaults: Defaults) -> bool {
        let kind = context["turnKind"].as_str().unwrap_or("primary");
        if defaults
            .required_turn
            .is_some_and(|expected| kind != expected)
        {
            return false;
        }
        let (turn_default, request_default) = (defaults.turn, defaults.request);
        let session = if context
            .get("parentID")
            .or_else(|| context.get("parentSessionID"))
            .is_some_and(Value::is_string)
            || context["sessionKind"] == "child"
        {
            "child"
        } else {
            "root"
        };
        let model = if let Some(model) = context["model"].as_str() {
            model.to_owned()
        } else if context["model"]["providerID"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
            && context["model"]["modelID"]
                .as_str()
                .is_some_and(|s| !s.is_empty())
        {
            format!(
                "{}/{}",
                context["model"]["providerID"].as_str().unwrap_or(""),
                context["model"]["modelID"].as_str().unwrap_or("")
            )
        } else {
            String::new()
        };
        if self.model.is_some() && model.is_empty() {
            return false;
        }
        for (pattern, value) in [
            (&self.directory, context["directory"].as_str().unwrap_or("")),
            (&self.session, session),
            (&self.agent, context["agent"].as_str().unwrap_or("")),
            (&self.model, model.as_str()),
            (
                &self.tool,
                context["tool"]
                    .as_str()
                    .or(context["toolName"].as_str())
                    .unwrap_or(""),
            ),
            (
                &self.outcome,
                context["outcome"]
                    .as_str()
                    .or(context["reason"].as_str())
                    .unwrap_or(""),
            ),
        ] {
            if pattern.as_ref().is_some_and(|p| !p.matches(value)) {
                return false;
            }
        }
        match &self.turn_kind {
            Some(p) if !p.matches(kind) => return false,
            None if turn_default.is_some_and(|d| d != kind) => return false,
            _ => {}
        }
        let request = context["requestKind"]
            .as_str()
            .unwrap_or(defaults.request.unwrap_or("primary"));
        match &self.request_kind {
            Some(p) if !p.matches(request) => return false,
            None if request_default.is_some_and(|d| d != request) => return false,
            _ => {}
        }
        true
    }
}

pub fn catalog() -> Value {
    let hooks = declarations().map(Hook::catalog).collect::<Vec<_>>();
    json!({"version":1,"hooks":hooks,"events":EVENT_NAMES,"matcherSemantics":"exact strings or simple glob patterns (*, ?, character classes); absent generic model/text requestKind and turn turnKind default to primary; dedicated compaction hooks default to compaction requestKind; resources have no implicit kind filter; explicit model matchers require a known model","failurePolicies":["block","continue"],"observerDelivery":{"modes":["bestEffort","durable"],"durableEvents":["ocmini.turn.ended","command.executed"],"overflow":["dropNewest","dropOldest","coalesce"]},"payloadProtocols":{"inline":{"version":1},"file":{"version":2,"inputSchema":{"type":"object","required":["version","invocationID","payload"],"properties":{"version":{"const":2},"invocationID":{"type":"string"},"payload":{"type":"object","required":["path","bytes","contentType","lifetime"],"properties":{"path":{"type":"string"},"bytes":{"type":"integer","minimum":0},"contentType":{"const":"application/json"},"lifetime":{"const":"invocation"}},"additionalProperties":false}},"additionalProperties":false},"responseSchema":{"type":"object","required":["version","responseFile"],"properties":{"version":{"const":2},"responseFile":{"type":"string"}},"additionalProperties":false}}},"observerResponseSchema":{"type":"object","properties":{"version":{"const":1},"ack":{"const":true}},"additionalProperties":false}})
}
