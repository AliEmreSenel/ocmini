use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const PROTECTED_TOOL_METADATA: &[&str] = &[
    "exit",
    "status",
    "childID",
    "sessionId",
    "sessionID",
    "model",
    "callID",
    "outputPath",
    "truncated",
    "diff",
    "patch",
    "attachments",
    "files",
    "outputBytes",
    "time",
    "usage",
    "background",
    "pid",
    "timeout",
    "logPath",
];

pub const HOOK_NAMES: [&str; 26] = [
    "turn.start.before",
    "turn.end.before",
    "turn.end.after",
    "tool.execute.before",
    "tool.execute.after",
    "tool.catalog.transform",
    "shell.env",
    "permission.decide",
    "question.ask.before",
    "model.context.transform",
    "model.params.transform",
    "model.headers.transform",
    "model.retry.before",
    "message.text.complete",
    "compaction.before",
    "compaction.after",
    "prompt.accept.before",
    "command.execute.before",
    "command.expand.after",
    "session.create.before",
    "session.update.before",
    "session.fork.before",
    "session.delete.before",
    "task.spawn.before",
    "skill.load.after",
    "file.change.before",
];
pub const EVENT_NAMES: [&str; 49] = [
    "server.connected",
    "server.heartbeat",
    "session.created",
    "session.updated",
    "session.deleted",
    "session.status",
    "session.idle",
    "session.error",
    "session.compacted",
    "session.title",
    "message.updated",
    "message.part.updated",
    "message.part.delta",
    "todo.updated",
    "permission.asked",
    "permission.replied",
    "question.asked",
    "question.replied",
    "question.rejected",
    "ocmini.runtime.started",
    "ocmini.runtime.stopping",
    "ocmini.config.loaded",
    "ocmini.hook.started",
    "ocmini.hook.completed",
    "ocmini.hook.failed",
    "ocmini.hook.queue.overflow",
    "ocmini.prompt.accepted",
    "ocmini.turn.started",
    "ocmini.turn.end.requested",
    "ocmini.turn.continued",
    "ocmini.turn.ended",
    "ocmini.tool.started",
    "ocmini.tool.completed",
    "ocmini.tool.failed",
    "ocmini.permission.decided",
    "ocmini.model.started",
    "ocmini.model.completed",
    "ocmini.model.failed",
    "ocmini.model.retrying",
    "ocmini.compaction.started",
    "ocmini.compaction.failed",
    "ocmini.prompt.rejected",
    "command.executed",
    "ocmini.session.forked",
    "ocmini.task.started",
    "ocmini.task.ended",
    "ocmini.skill.loaded",
    "file.edited",
    "ocmini.mcp.status",
];

pub fn mutable_keys(name: &str) -> &'static [&'static str] {
    match name {
        "turn.start.before" => &["context"],
        "tool.execute.before" => &["args"],
        "tool.execute.after" => &["output", "title", "metadata"],
        "tool.catalog.transform" => &["tools"],
        "shell.env" => &["env"],
        "permission.decide" => &["decision"],
        "question.ask.before" => &["questions"],
        "model.context.transform" => &["system", "messages"],
        "model.params.transform" => &["options", "variant"],
        "model.headers.transform" => &["headers"],
        "model.retry.before" => &["delayMs"],
        "message.text.complete" => &["text"],
        "compaction.before" => &["context", "prompt"],
        "compaction.after" => &["summary"],
        "prompt.accept.before" | "command.expand.after" => &["parts"],
        "command.execute.before" => &["arguments"],
        "session.create.before" | "session.update.before" => &["title", "metadata"],
        "session.fork.before" => &["title"],
        "task.spawn.before" => &["prompt", "description", "subagent_type", "model"],
        "skill.load.after" => &["content"],
        "file.change.before" => &["changes"],
        _ => &[],
    }
}
pub fn is_admission(name: &str) -> bool {
    matches!(
        name,
        "prompt.accept.before" | "command.execute.before" | "command.expand.after"
    )
}
pub fn default_kind(name: &str) -> (Option<&'static str>, Option<&'static str>) {
    if name.starts_with("turn.") {
        (Some("primary"), None)
    } else if name.starts_with("model.") || name == "message.text.complete" {
        (None, Some("primary"))
    } else if name.starts_with("compaction.") {
        (None, Some("compaction"))
    } else {
        (None, None)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Patterns {
    One(String),
    Many(Vec<String>),
}
impl Patterns {
    pub fn values(&self) -> Vec<&str> {
        match self {
            Self::One(s) => vec![s],
            Self::Many(v) => v.iter().map(String::as_str).collect(),
        }
    }
    pub fn matches(&self, value: &str) -> bool {
        self.values()
            .iter()
            .any(|p| glob::Pattern::new(p).is_ok_and(|p| p.matches(value)))
    }
    pub fn validate(&self) -> Result<(), String> {
        let values = self.values();
        if values.is_empty() {
            return Err("a matcher cannot be empty".into());
        }
        for value in values {
            glob::Pattern::new(value).map_err(|e| format!("invalid matcher {value}: {e}"))?;
        }
        Ok(())
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
    pub fn matches(&self, name: &str, context: &Value, defaults: bool) -> bool {
        let kind = context["turnKind"].as_str().unwrap_or("primary");
        if name == "turn.end.before" && kind != "primary" {
            return false;
        }
        let (turn_default, request_default) = if defaults {
            default_kind(name)
        } else {
            (None, None)
        };
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
        let request =
            context["requestKind"]
                .as_str()
                .unwrap_or(if name.starts_with("compaction.") {
                    "compaction"
                } else {
                    "primary"
                });
        match &self.request_kind {
            Some(p) if !p.matches(request) => return false,
            None if request_default.is_some_and(|d| d != request) => return false,
            _ => {}
        }
        true
    }
}

pub fn catalog() -> Value {
    let hooks=HOOK_NAMES.iter().map(|name| {
        let (turn,request)=default_kind(name);
        let fields:serde_json::Map<String,Value>=mutable_keys(name).iter().map(|key| {
            let schema=match *key {
                "args"|"options"|"headers"|"env"|"metadata"=>json!({"type":"object"}),
                "tools"|"questions"|"messages"|"parts"|"changes"=>json!({"type":"array"}),
                "context" if *name=="compaction.before"=>json!({"type":"array","items":{"type":"string"}}),
                "variant"=>json!({"type":["string","null"]}),
                "prompt" if *name=="compaction.before"=>json!({"type":["string","null"]}),
                "model"=>json!({"type":"object","required":["providerID","modelID"],"properties":{"providerID":{"type":"string"},"modelID":{"type":"string"}},"additionalProperties":false}),
                "delayMs"=>json!({"type":"integer","minimum":0,"maximum":120000}),
                "decision"=>json!({"enum":["allow","ask","deny"]}),
                _=>json!({"type":"string"}),
            };(key.to_string(),schema)
        }).collect();
        let set_schema=json!({"type":"object","properties":fields,"additionalProperties":false});
        let decisions=if *name=="turn.end.before" {json!(["allow","block","continue"])}else{json!(["allow","block"])};
        let response=if *name=="turn.end.after" {json!({"type":"object","properties":{"version":{"const":1}},"additionalProperties":false})} else {json!({"type":"object","properties":{"version":{"const":1},"decision":{"enum":decisions},"set":set_schema,"feedback":{"type":"string","minLength":1},"reason":{"type":"string"}},"additionalProperties":false})};
        json!({"name":name,"mutableKeys":mutable_keys(name),"setSchema":set_schema,"responseSchema":response,"defaultTurnKind":turn,"defaultRequestKind":request,"continuation":*name=="turn.end.before","readOnly":*name=="turn.end.after"})
    }).collect::<Vec<_>>();
    json!({"version":1,"hooks":hooks,"events":&EVENT_NAMES[..],"matcherSemantics":"exact strings or simple glob patterns (*, ?, character classes); absent generic model/text requestKind and turn turnKind default to primary; dedicated compaction hooks default to compaction requestKind; resources have no implicit kind filter; explicit model matchers require a known model","failurePolicies":["block","continue"],"observerDelivery":{"modes":["bestEffort","durable"],"durableEvents":["ocmini.turn.ended","command.executed"],"overflow":["dropNewest","dropOldest","coalesce"]},"payloadProtocols":{"inline":{"version":1},"file":{"version":2,"inputSchema":{"type":"object","required":["version","invocationID","payload"],"properties":{"version":{"const":2},"invocationID":{"type":"string"},"payload":{"type":"object","required":["path","bytes","contentType","lifetime"],"properties":{"path":{"type":"string"},"bytes":{"type":"integer","minimum":0},"contentType":{"const":"application/json"},"lifetime":{"const":"invocation"}},"additionalProperties":false}},"additionalProperties":false},"responseSchema":{"type":"object","required":["version","responseFile"],"properties":{"version":{"const":2},"responseFile":{"type":"string"}},"additionalProperties":false}}},"observerResponseSchema":{"type":"object","properties":{"version":{"const":1},"ack":{"const":true}},"additionalProperties":false}})
}

#[derive(Debug)]
pub struct HookResult {
    pub input: Value,
    pub feedback: Option<String>,
}

pub fn apply_response(
    name: &str,
    response: Value,
    input: &mut Value,
) -> Result<Option<String>, String> {
    let Value::Object(mut object) = response else {
        return Err("hook stdout must be one JSON object".into());
    };
    if object
        .keys()
        .any(|key| !["version", "decision", "set", "feedback", "reason"].contains(&key.as_str()))
    {
        return Err("unknown hook response field".into());
    }
    if object.get("version").is_some_and(|v| v != 1) {
        return Err("unsupported hook response version".into());
    }
    if name == "turn.end.after" {
        if object.keys().any(|key| key != "version") {
            return Err("turn.end.after is read-only".into());
        }
        return Ok(None);
    }
    let decision = match object.get("decision") {
        None => "allow",
        Some(v) => v.as_str().ok_or("decision must be a string")?,
    };
    if !["allow", "block", "continue"].contains(&decision) {
        return Err("unknown hook decision".into());
    }
    if decision == "continue" && name != "turn.end.before" {
        return Err("continue is only valid for turn.end.before".into());
    }
    if decision != "continue" && object.contains_key("feedback") {
        return Err("feedback requires a continue decision".into());
    }
    if decision != "block" && object.contains_key("reason") {
        return Err("reason requires a block decision".into());
    }
    if decision != "allow" && object.contains_key("set") {
        return Err("set cannot accompany block or continue".into());
    }
    if decision == "block" {
        let reason = object
            .get("reason")
            .map(|v| v.as_str().ok_or("reason must be a string"))
            .transpose()?
            .unwrap_or("blocked by hook");
        return Err(format!(
            "hook rejected: {}",
            reason.chars().take(2048).collect::<String>()
        ));
    }
    if decision == "continue" {
        let feedback = object
            .get("feedback")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or("continue requires nonempty feedback")?;
        return Ok(Some(feedback.to_owned()));
    }
    if let Some(set_value) = object.remove("set") {
        let set = set_value.as_object().ok_or("set must be an object")?;
        if name == "tool.execute.after"
            && (input.get("error").is_some_and(|v| !v.is_null())
                || input.get("success") == Some(&Value::Bool(false)))
            && !set.is_empty()
        {
            return Err("failed tool results are read-only".into());
        }
        for (key, value) in set {
            if !mutable_keys(name).contains(&key.as_str()) {
                return Err(format!("immutable or unknown field {key} for {name}"));
            }
            validate_replacement(name, key, value)?;
            if name == "permission.decide" && key == "decision" {
                let rank = |value: &Value| match value.as_str() {
                    Some("allow") => 0,
                    Some("ask") => 1,
                    _ => 2,
                };
                if rank(value) < rank(&input["decision"]) {
                    return Err("permission hooks can only tighten decisions".into());
                }
            }
            if name == "tool.execute.after" && key == "metadata" {
                for &field in PROTECTED_TOOL_METADATA {
                    if input["metadata"].get(field) != value.get(field) {
                        return Err(format!(
                            "protected runtime metadata {field} cannot be changed"
                        ));
                    }
                }
            }
        }
        let input = input
            .as_object_mut()
            .ok_or("hook input must be an object for replacements")?;
        // Validation above is atomic; move only the changed values after every
        // replacement has passed, avoiding another copy of large file content.
        let Value::Object(set) = set_value else {
            unreachable!()
        };
        for (key, value) in set {
            input.insert(key, value);
        }
    }
    Ok(None)
}
fn validate_replacement(name: &str, key: &str, value: &Value) -> Result<(), String> {
    let valid = match key {
        "args" | "options" | "headers" | "env" | "metadata" => value.is_object(),
        "tools" | "questions" | "messages" | "parts" | "changes" => value.is_array(),
        "context" if name == "compaction.before" => value
            .as_array()
            .is_some_and(|v| v.iter().all(Value::is_string)),
        "variant" | "prompt" if name == "compaction.before" || key == "variant" => {
            value.is_null() || value.is_string()
        }
        "model" => value.as_object().is_some_and(|m| {
            m.len() == 2
                && m.get("providerID").is_some_and(Value::is_string)
                && m.get("modelID").is_some_and(Value::is_string)
        }),
        "delayMs" => value.as_u64().is_some_and(|n| n <= 120_000),
        "decision" => value
            .as_str()
            .is_some_and(|s| ["allow", "ask", "deny"].contains(&s)),
        _ => value.is_string(),
    };
    if !valid {
        return Err(format!("invalid replacement type for {name}.{key}"));
    }
    if key == "headers"
        && value.as_object().unwrap().iter().any(|(key, value)| {
            let key = key.to_ascii_lowercase();
            key.contains("authorization")
                || key.contains("api-key")
                || key == "cookie"
                || key == "set-cookie"
                || key == "host"
                || key == "content-length"
                || !value.is_string()
        })
    {
        return Err("authentication or transport headers cannot be changed".into());
    }
    if key == "env"
        && value.as_object().unwrap().iter().any(|(key, value)| {
            key.is_empty()
                || key.starts_with("OCMINI_HOOK_")
                || key.contains(['=', '\0'])
                || !(value.is_string() || value.is_null())
                || value.as_str().is_some_and(|v| v.contains('\0'))
        })
    {
        return Err("invalid environment replacement".into());
    }

    Ok(())
}
