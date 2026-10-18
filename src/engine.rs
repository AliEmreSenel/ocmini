use self::question::QuestionState;
use crate::auth::AuthStore;
use crate::bus::Bus;
use crate::config::{Config, McpLocal};
use crate::mcp::McpHub;
use crate::permission::PermissionState;
use crate::store::Store;
use crate::types::{ModelRef, PromptRequest};
use crate::usage_db::UsageDb;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once, OnceLock};
use tokio::sync::oneshot;

pub mod question {
    use crate::types::QuestionRequest;
    use serde_json::{json, Value};
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
        ) -> (QuestionRequest, oneshot::Receiver<Answer>) {
            let (tx, rx) = oneshot::channel();
            let req = QuestionRequest {
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
            let pending = self.pending.lock().unwrap();
            let Some((req, _)) = pending.iter().find(|(r, _)| r.id == id) else {
                return false;
            };
            if answers.len() != req.questions.len() {
                return false;
            }
            for (answer, question) in answers.iter().zip(&req.questions) {
                if question["multiple"] != true && answer.len() > 1 {
                    return false;
                }
            }
            drop(pending);
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
            let (req, tx) = p.remove(i);
            drop(p);
            let (event, properties) = match &answers {
                Some(a) => (
                    "question.replied",
                    json!({"sessionID":req.session_id,"requestID":req.id,"answers":a}),
                ),
                None => (
                    "question.rejected",
                    json!({"sessionID":req.session_id,"requestID":req.id}),
                ),
            };
            crate::engine::bus().emit(event, properties);
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
    ready: Arc<AtomicBool>,
}

impl McpScope {
    fn new(config: &Config, directory: &str) -> Self {
        Self {
            hub: Arc::new(McpHub::new()),
            entries: config.mcp.clone(),
            directory: directory.to_string(),
            started: Once::new(),
            ready: Arc::new(AtomicBool::new(config.mcp.is_empty())),
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
            std::thread::spawn(move || {
                for (name, entry) in entries {
                    let mut value = serde_json::to_value(entry).expect("MCP config JSON");
                    if value["cwd"].as_str().is_none_or(str::is_empty) {
                        value["cwd"] = serde_json::json!(directory);
                    }
                    let _ = background.add(&name, value);
                }
                done.store(true, Ordering::Release);
            });
        });
    }
}

#[derive(Clone)]
pub struct App {
    pub store: Arc<Store>,
    pub config: Arc<Mutex<Config>>,
    pub auth: Arc<AuthStore>,
    pub perms: PermissionState,
    pub questions: QuestionState,
    pub mcp: Arc<McpHub>,
    mcp_scope: Arc<McpScope>,
    mcp_scopes: Arc<Mutex<std::collections::HashMap<String, Arc<McpScope>>>>,
    pub usage: Arc<UsageDb>,
    pub always: Arc<Mutex<std::collections::HashMap<String, Vec<crate::types::PermissionRule>>>>,
    pub statuses: Arc<Mutex<std::collections::HashMap<String, serde_json::Value>>>,
}

impl App {
    pub fn new() -> Result<Self, String> {
        let config = Config::load()?;
        crate::tools::cleanup_outputs()?;
        let directory = std::env::current_dir().unwrap_or_default();
        let directory = directory.to_string_lossy().into_owned();
        let mcp_scope = Arc::new(McpScope::new(&config, &directory));
        let mcp_scopes = Arc::new(Mutex::new(std::collections::HashMap::from([(
            directory,
            mcp_scope.clone(),
        )])));
        Ok(Self {
            store: Arc::new(Store::open(
                &crate::config::store_path(),
                &crate::types::hex_id("ocm_"),
            )?),
            config: Arc::new(Mutex::new(config)),
            auth: Arc::new(AuthStore::load()),
            perms: PermissionState::default(),
            questions: QuestionState::default(),
            mcp: mcp_scope.hub.clone(),
            mcp_scope,
            mcp_scopes,
            usage: Arc::new(UsageDb::open()),
            always: Arc::new(Mutex::new(Default::default())),
            statuses: Arc::new(Mutex::new(Default::default())),
        })
    }

    pub fn for_directory(&self, directory: &str) -> Result<Self, String> {
        let config = Config::load_for(directory)?;
        let mut scopes = self.mcp_scopes.lock().unwrap();
        let scope = scopes
            .entry(directory.to_string())
            .or_insert_with(|| Arc::new(McpScope::new(&config, directory)))
            .clone();
        let mut scoped = self.clone();
        scoped.mcp = scope.hub.clone();
        scoped.mcp_scope = scope;
        scoped.config = Arc::new(Mutex::new(config));
        Ok(scoped)
    }

    pub async fn wait_mcp_ready(&self) {
        self.mcp_scope.start();
        while !self.mcp_scope.ready.load(Ordering::Acquire) {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
}

pub trait EngineApi: Send + Sync + 'static {
    fn start_turn(
        &self,
        app: App,
        session_id: String,
        request: PromptRequest,
    ) -> Result<oneshot::Receiver<serde_json::Value>, String>;
    fn abort(&self, session_id: &str);
    fn run_sync(
        &self,
        app: App,
        session_id: String,
        request: PromptRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<serde_json::Value, String>> + Send>,
    >;
    fn summarize(
        &self,
        app: App,
        session_id: String,
        model: Option<ModelRef>,
    ) -> Result<oneshot::Receiver<serde_json::Value>, String>;
    fn recover(&self, app: App);
}

pub fn bus() -> &'static Bus {
    static BUS: OnceLock<Bus> = OnceLock::new();
    BUS.get_or_init(Bus::new)
}
