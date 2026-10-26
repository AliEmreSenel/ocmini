use crate::events as event;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Domain identities and wire names come from one closed hook tree.
macro_rules! hook_names {
    ($($group:ident($domain:ident) { $($leaf:ident => $wire:literal),+ $(,)? }),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(untagged)]
        pub enum HookName { $($group($domain)),+ }
        impl HookName {
            pub const ALL: &'static [Self] = &[$($(Self::$group($domain::$leaf)),+),+];
            pub fn as_str(self) -> &'static str { match self { $(Self::$group(hook) => hook.as_str()),+ } }
        }
        $(
            #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
            pub enum $domain { $(#[serde(rename=$wire)] $leaf),+ }
            impl $domain {
                fn as_str(self) -> &'static str { match self { $(Self::$leaf => $wire),+ } }
            }
        )+
        #[cfg(test)]
        pub const HOOK_NAMES: &[&str] = &[$($($wire),+),+];
    };
}
hook_names! {
    Turn(TurnHook) {
        StartBefore => "turn.start.before",
        EndBefore => "turn.end.before",
        EndAfter => "turn.end.after"
    },
    Tool(ToolHook) {
        ExecuteBefore => "tool.execute.before",
        ExecuteAfter => "tool.execute.after",
        CatalogTransform => "tool.catalog.transform"
    },
    Shell(ShellHook) {
        Env => "shell.env"
    },
    Permission(PermissionHook) {
        Decide => "permission.decide"
    },
    Question(QuestionHook) {
        AskBefore => "question.ask.before"
    },
    Model(ModelHook) {
        ContextTransform => "model.context.transform",
        ParamsTransform => "model.params.transform",
        HeadersTransform => "model.headers.transform",
        RetryBefore => "model.retry.before"
    },
    Message(MessageHook) {
        TextComplete => "message.text.complete"
    },
    Compaction(CompactionHook) {
        Before => "compaction.before",
        After => "compaction.after"
    },
    Prompt(PromptHook) {
        AcceptBefore => "prompt.accept.before"
    },
    Command(CommandHook) {
        ExecuteBefore => "command.execute.before",
        ExpandAfter => "command.expand.after"
    },
    Session(SessionHook) {
        Create => "session.create.before",
        Update => "session.update.before",
        Fork => "session.fork.before",
        Delete => "session.delete.before"
    },
    Task(TaskHook) {
        SpawnBefore => "task.spawn.before"
    },
    Skill(SkillHook) {
        LoadAfter => "skill.load.after"
    },
    File(FileHook) {
        ChangeBefore => "file.change.before"
    }
}
pub use event::NAMES as EVENT_NAMES;

#[derive(Clone, Copy, Debug, Default)]
pub struct Defaults {
    pub turn: Option<&'static str>,
    pub request: Option<&'static str>,
    pub required_turn: Option<&'static str>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InvocationKind {
    #[default]
    Handler,
    Admission,
    Finalizer,
    Observer,
}

#[derive(Clone, Copy)]
pub enum Shape {
    Text,
    NullableText,
    Object,
    Array,
    TextArray,
    Integer(u64, u64),
    Choice(&'static [&'static str]),
    Record(&'static [Field]),
}
pub type Field = (&'static str, Shape);
impl Shape {
    fn accepts(self, value: &Value) -> bool {
        match self {
            Self::Text => value.is_string(),
            Self::NullableText => value.is_null() || value.is_string(),
            Self::Object => value.is_object(),
            Self::Array => value.is_array(),
            Self::TextArray => value
                .as_array()
                .is_some_and(|v| v.iter().all(Value::is_string)),
            Self::Integer(min, max) => value.as_u64().is_some_and(|n| (min..=max).contains(&n)),
            Self::Choice(values) => value.as_str().is_some_and(|s| values.contains(&s)),
            Self::Record(fields) => value.as_object().is_some_and(|m| {
                m.len() == fields.len()
                    && fields
                        .iter()
                        .all(|(key, shape)| m.get(*key).is_some_and(|v| shape.accepts(v)))
            }),
        }
    }
    fn schema(self) -> Value {
        match self {
            Self::Text => json!({"type":"string"}),
            Self::NullableText => json!({"type":["string","null"]}),
            Self::Object => json!({"type":"object"}),
            Self::Array => json!({"type":"array"}),
            Self::TextArray => json!({"type":"array","items":{"type":"string"}}),
            Self::Integer(min, max) => json!({"type":"integer","minimum":min,"maximum":max}),
            Self::Choice(values) => json!({"enum":values}),
            Self::Record(fields) => {
                json!({"type":"object","required":fields.iter().map(|f|f.0).collect::<Vec<_>>(),"properties":field_schemas(fields),"additionalProperties":false})
            }
        }
    }
}
fn field_schemas(fields: &[Field]) -> serde_json::Map<String, Value> {
    fields
        .iter()
        .map(|(key, shape)| (key.to_string(), shape.schema()))
        .collect()
}

pub type Validator = fn(&Value, &serde_json::Map<String, Value>) -> Result<(), String>;
#[derive(Clone, Copy)]
pub struct Hook {
    pub name: HookName,
    pub fields: &'static [Field],
    pub defaults: Defaults,
    pub kind: InvocationKind,
    continuation: bool,
    validate: Validator,
}
fn valid(_: &Value, _: &serde_json::Map<String, Value>) -> Result<(), String> {
    Ok(())
}
impl Hook {
    pub const fn new(name: HookName, fields: &'static [Field]) -> Self {
        Self {
            name,
            fields,
            defaults: Defaults {
                turn: None,
                request: None,
                required_turn: None,
            },
            kind: InvocationKind::Handler,
            continuation: false,
            validate: valid,
        }
    }
    pub const fn request(mut self, kind: &'static str) -> Self {
        self.defaults.request = Some(kind);
        self
    }
    pub const fn turn(mut self, kind: &'static str) -> Self {
        self.defaults.turn = Some(kind);
        self
    }
    pub const fn gate(mut self) -> Self {
        self.continuation = true;

        self
    }
    pub const fn only_turn(mut self, kind: &'static str) -> Self {
        self.defaults.required_turn = Some(kind);
        self
    }
    pub const fn kind(mut self, kind: InvocationKind) -> Self {
        self.kind = kind;
        self
    }
    pub const fn validate(mut self, validate: Validator) -> Self {
        self.validate = validate;
        self
    }
    pub fn chain_timeout(self, settings: &super::config::Settings) -> Option<u64> {
        match self.kind {
            InvocationKind::Admission => Some(settings.admission_chain_timeout_ms),
            InvocationKind::Finalizer => settings.finalizer_chain_timeout_ms,
            _ => settings.chain_timeout_ms,
        }
    }
    #[cfg(test)]
    pub fn apply(self, response: Value, input: &mut Value) -> Result<Option<String>, String> {
        self.apply_with(response, input, &mut |_, _| Ok(()))
    }
    pub fn apply_with(
        self,
        response: Value,
        input: &mut Value,
        validate: &mut impl FnMut(&Value, &mut serde_json::Map<String, Value>) -> Result<(), String>,
    ) -> Result<Option<String>, String> {
        if response
            .as_object()
            .is_some_and(|object| object.values().any(Value::is_null))
        {
            return Err("hook response fields cannot be null".into());
        }
        let response: Reply =
            serde_json::from_value(response).map_err(|e| format!("invalid hook response: {e}"))?;
        if response.version.is_some_and(|v| v != 1) {
            return Err("unsupported hook response version".into());
        }
        if self.kind == InvocationKind::Finalizer {
            if response.decision.is_some()
                || response.set.is_some()
                || response.feedback.is_some()
                || response.reason.is_some()
            {
                return Err("finalizer is read-only".into());
            }
            return Ok(None);
        }
        match response.decision.unwrap_or(Decision::Allow) {
            Decision::Block if response.set.is_none() && response.feedback.is_none() => {
                Err(format!(
                    "hook rejected: {}",
                    response
                        .reason
                        .as_deref()
                        .unwrap_or("blocked by hook")
                        .chars()
                        .take(2048)
                        .collect::<String>()
                ))
            }
            Decision::Continue
                if self.continuation && response.reason.is_none() && response.set.is_none() =>
            {
                response
                    .feedback
                    .filter(|s| !s.trim().is_empty())
                    .map(Some)
                    .ok_or_else(|| "continue requires nonempty feedback".into())
            }
            Decision::Allow if response.reason.is_none() && response.feedback.is_none() => {
                if let Some(mut set) = response.set {
                    for (key, value) in &set {
                        let shape = self
                            .fields
                            .iter()
                            .find(|f| f.0 == key)
                            .ok_or_else(|| format!("immutable or unknown field {key}"))?
                            .1;
                        if !shape.accepts(value) {
                            return Err(format!("invalid replacement for {key}"));
                        }
                    }
                    (self.validate)(input, &set)?;
                    validate(input, &mut set)?;
                    let input = input
                        .as_object_mut()
                        .ok_or("hook input must be an object")?;
                    input.extend(set);
                }
                Ok(None)
            }
            _ => Err("invalid hook decision or accompanying fields".into()),
        }
    }
    pub fn catalog(self) -> Value {
        let set = json!({"type":"object","properties":field_schemas(self.fields),"additionalProperties":false});
        let decisions = if self.continuation {
            json!(["allow", "block", "continue"])
        } else {
            json!(["allow", "block"])
        };
        let response = if self.kind == InvocationKind::Finalizer {
            json!({"type":"object","properties":{"version":{"const":1}},"additionalProperties":false})
        } else {
            json!({"type":"object","properties":{"version":{"const":1},"decision":{"enum":decisions},"set":set,"feedback":{"type":"string","minLength":1},"reason":{"type":"string"}},"additionalProperties":false})
        };
        json!({"name":self.name.as_str(),"mutableKeys":self.fields.iter().map(|f|f.0).collect::<Vec<_>>(),"setSchema":set,"responseSchema":response,"defaultTurnKind":self.defaults.turn,"defaultRequestKind":self.defaults.request,"continuation":self.continuation,"readOnly":self.kind==InvocationKind::Finalizer})
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Reply {
    version: Option<u32>,
    decision: Option<Decision>,
    set: Option<serde_json::Map<String, Value>>,
    feedback: Option<String>,
    reason: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Decision {
    Allow,
    Block,
    Continue,
}

#[derive(Debug)]
pub struct HookResult {
    pub input: Value,
    pub feedback: Option<String>,
}

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
