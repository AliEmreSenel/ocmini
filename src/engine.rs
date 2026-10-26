use self::question::QuestionState;
use crate::auth::AuthStore;
use crate::bus::Bus;
use crate::config::{Config, McpLocal};
use crate::events as event;
use crate::mcp::McpHub;
use crate::permission::PermissionState;
use crate::store::Store;
use crate::types::{MessageWithParts, ModelRef, OperationError, PromptRequest, TurnResult};
use crate::usage_db::UsageDb;
use futures::StreamExt;
use std::sync::{Arc, Mutex, Once, OnceLock};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

pub mod question {
    use crate::events as event;
    use crate::types::QuestionRequest;
    use serde_json::Value;
    use std::sync::{Arc, Mutex};
    use tokio::sync::oneshot;

    type Answer = Option<Vec<Vec<String>>>;
    type Pending = (QuestionRequest, oneshot::Sender<Answer>);

    #[derive(Clone, Default)]
    pub struct QuestionState {
        pending: Arc<Mutex<Vec<Pending>>>,
    }

    impl QuestionState {
        pub fn ask_for_tool(
            &self,
            session_id: &str,
            questions: Value,
            tool: Option<Value>,
            context: Value,
        ) -> (QuestionRequest, oneshot::Receiver<Answer>) {
            let (tx, rx) = oneshot::channel();
            let req = QuestionRequest {
                context,
                id: crate::types::hex_id("que_"),
                session_id: session_id.into(),
                questions: questions
                    .as_array()
                    .cloned()
                    .unwrap_or_else(|| vec![questions]),
                tool,
            };
            self.pending.lock().unwrap().push((req.clone(), tx));
            (req, rx)
        }

        pub fn reply(&self, id: &str, answers: Vec<Vec<String>>) -> bool {
            self.finish(id, Some(answers))
        }

        pub fn reject(&self, id: &str) -> bool {
            self.finish(id, None)
        }

        fn finish(&self, id: &str, answers: Answer) -> bool {
            let mut p = self.pending.lock().unwrap();
            let Some(i) = p.iter().position(|(r, _)| r.id == id) else {
                return false;
            };
            if answers
                .as_ref()
                .is_some_and(|answers| p[i].0.validate_answers(answers).is_err())
            {
                return false;
            }
            let (req, tx) = p.remove(i);
            drop(p);
            let event = match &answers {
                Some(answers) => event::Question::Replied {
                    session_id: &req.session_id,
                    request_id: &req.id,
                    answers,
                },
                None => event::Question::Rejected {
                    session_id: &req.session_id,
                    request_id: &req.id,
                },
            };
            crate::engine::bus().emit(event, req.context);
            let _ = tx.send(answers);
            true
        }

        pub fn reject_all(&self, sid: &str) {
            let ids: Vec<String> = self
                .list()
                .into_iter()
                .filter(|r| r.session_id == sid)
                .map(|r| r.id)
                .collect();
            for id in ids {
                self.reject(&id);
            }
        }

        pub fn list(&self) -> Vec<QuestionRequest> {
            self.pending
                .lock()
                .unwrap()
                .iter()
                .map(|(r, _)| r.clone())
                .collect()
        }
    }
}

struct McpScope {
    hub: Arc<McpHub>,
    entries: std::collections::HashMap<String, McpLocal>,
    directory: String,
    started: Once,
    ready: tokio::sync::watch::Sender<bool>,
}

impl McpScope {
    fn new(config: &Config, directory: &str, hooks: Arc<crate::hooks::Hooks>) -> Self {
        let hub = Arc::new(McpHub::new());
        hub.set_hooks(hooks, directory);
        let (ready, _) = tokio::sync::watch::channel(config.mcp.is_empty());
        Self {
            hub,
            entries: config.mcp.clone(),
            directory: directory.to_string(),
            started: Once::new(),
            ready,
        }
    }

    fn start(&self) {
        // Selecting a directory (including the launch directory) must not
        // start its MCP processes. Initialize once, when MCP is first used.
        self.started.call_once(|| {
            if self.entries.is_empty() {
                return;
            }
            let entries = self.entries.clone();
            let directory = self.directory.clone();
            let background = self.hub.clone();
            let done = self.ready.clone();
            // Keep initialization alive if the first request is cancelled.
            tokio::spawn(async move {
                futures::stream::iter(entries)
                    .for_each_concurrent(8, |(name, entry)| {
                        let (hub, directory) = (background.clone(), directory.clone());
                        async move {
                            let mut value = serde_json::to_value(entry).expect("MCP config JSON");
                            if value["cwd"].as_str().is_none_or(str::is_empty) {
                                value["cwd"] = serde_json::json!(directory);
                            }
                            let _ = hub.add_async(&name, value).await;
                        }
                    })
                    .await;
                done.send_replace(true);
            });
        });
    }
}

#[derive(Clone)]
pub struct App {
    pub store: Arc<Store>,
    pub config: Arc<Config>,
    pub auth: Arc<AuthStore>,
    pub perms: PermissionState,
    pub questions: QuestionState,
    mcp_scope: Arc<McpScope>,
    mcp_scopes: Arc<Mutex<std::collections::HashMap<String, Arc<McpScope>>>>,
    pub usage: Arc<UsageDb>,
    pub statuses: Arc<Mutex<std::collections::HashMap<String, event::SessionStatus>>>,
    pub hooks: Arc<crate::hooks::Hooks>,
    pub hook_runtime: Arc<crate::hooks::Runtime>,
}

impl App {
    pub fn new() -> Result<Self, String> {
        let config = Config::load()?;
        crate::tools::cleanup_outputs()?;
        let directory = std::env::current_dir().unwrap_or_default();
        let directory = directory.to_string_lossy().into_owned();
        let hook_runtime = crate::hooks::Runtime::new();
        let hooks = hook_runtime.install(&config, &directory)?;
        let store = Arc::new(Store::open(
            &crate::config::store_path(),
            &crate::types::hex_id("ocm_"),
        )?);
        crate::hooks::recover(&store)?;
        hooks.attach_store(store.clone());
        let mcp_scope = Arc::new(McpScope::new(&config, &directory, hooks.clone()));
        let mcp_scopes = Arc::new(Mutex::new(std::collections::HashMap::from([(
            directory,
            mcp_scope.clone(),
        )])));
        Ok(Self {
            store,
            config: Arc::new(config),
            auth: Arc::new(AuthStore::load()),
            perms: PermissionState::default(),
            questions: QuestionState::default(),
            mcp_scope,
            mcp_scopes,
            usage: Arc::new(UsageDb::open()),
            statuses: Arc::new(Mutex::new(Default::default())),
            hooks,
            hook_runtime,
        })
    }

    pub fn for_directory(&self, directory: &str) -> Result<Self, OperationError> {
        let config = Config::load_for(directory).map_err(OperationError::invalid)?;
        let hooks = self
            .hook_runtime
            .install(&config, directory)
            .map_err(OperationError::invalid)?;
        hooks.attach_store(self.store.clone());
        let mut scopes = self.mcp_scopes.lock().unwrap();
        let retired = if scopes.len() >= 128 && !scopes.contains_key(directory) {
            let unused = scopes
                .iter()
                .find(|(_, scope)| Arc::strong_count(scope) == 1)
                .map(|(key, _)| key.clone());
            unused.and_then(|key| scopes.remove(&key))
        } else {
            None
        };
        let (scope, displaced) = match scopes
            .get(directory)
            .filter(|scope| scope.entries == config.mcp)
        {
            Some(scope) => (scope.clone(), None),
            None => {
                let scope = Arc::new(McpScope::new(&config, directory, hooks.clone()));
                let displaced = scopes.insert(directory.into(), scope.clone());
                (scope, displaced)
            }
        };
        drop(scopes);
        drop(retired);
        drop(displaced);
        scope.hub.set_hooks(hooks.clone(), directory);
        let mut scoped = self.clone();
        scoped.mcp_scope = scope;
        scoped.config = Arc::new(config);
        scoped.hooks = hooks;
        Ok(scoped)
    }

    /// Producers attach immutable session scope before delivery receives an envelope.
    pub fn session_context(&self, sid: &str, mut context: serde_json::Value) -> serde_json::Value {
        let session = self.store.scope(sid);
        context["sessionID"] = serde_json::json!(sid);
        context["directory"] =
            serde_json::json!(session.as_ref().map(|scope| scope.directory.as_str()));
        context["parentID"] = serde_json::json!(session.and_then(|scope| scope.parent_id));
        context
    }

    pub fn mcp(&self) -> &McpHub {
        &self.mcp_scope.hub
    }

    pub async fn wait_mcp_ready(&self) {
        self.mcp_scope.start();
        let mut ready = self.mcp_scope.ready.subscribe();
        let _ = ready.wait_for(|ready| *ready).await;
    }
}

#[async_trait::async_trait]
pub trait EngineApi: Send + Sync + 'static {
    fn begin_end_gate(
        &self,
        app: &App,
        session_id: &str,
    ) -> Result<Option<CancellationToken>, OperationError>;
    fn allow_hook_continuation(&self, app: &App, session_id: &str) -> Result<bool, OperationError>;
    fn finish_end_gate(&self, session_id: &str, token: &CancellationToken);
    async fn admit(
        &self,
        app: App,
        session_id: String,
        request: PromptRequest,
    ) -> Result<oneshot::Receiver<TurnResult>, OperationError>;
    fn start_turn(
        &self,
        app: App,
        session_id: String,
        request: PromptRequest,
    ) -> Result<oneshot::Receiver<TurnResult>, OperationError>;
    fn abort(&self, session_id: &str);
    async fn run_sync(
        &self,
        app: App,
        session_id: String,
        request: PromptRequest,
    ) -> Result<MessageWithParts, OperationError> {
        self.admit(app, session_id, request)
            .await?
            .await
            .map_err(|_| "Session execution stopped before a response was produced".to_string())?
    }
    fn summarize(
        &self,
        app: App,
        session_id: String,
        model: Option<ModelRef>,
    ) -> Result<oneshot::Receiver<TurnResult>, OperationError>;
    fn recover(&self, app: App);
}

pub fn bus() -> &'static Bus {
    static BUS: OnceLock<Bus> = OnceLock::new();
    BUS.get_or_init(Bus::new)
}
