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
    pub fn chain_timeout(
        self,
        admission: u64,
        finalizer: Option<u64>,
        normal: Option<u64>,
    ) -> Option<u64> {
        match self.kind {
            InvocationKind::Admission => Some(admission),
            InvocationKind::Finalizer => finalizer,
            _ => normal,
        }
    }
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
