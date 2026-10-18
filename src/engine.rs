use self::question::QuestionState;
use crate::auth::AuthStore;
use crate::bus::Bus;
use crate::config::Config;
use crate::mcp::McpHub;
use crate::permission::PermissionState;
use crate::store::Store;
use crate::types::{ModelRef, PromptRequest};
use crate::usage_db::UsageDb;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
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

type McpScope = (Arc<McpHub>, Arc<AtomicBool>);

#[derive(Clone)]
pub struct App {
    pub store: Arc<Store>,
    pub config: Arc<Mutex<Config>>,
    pub auth: Arc<AuthStore>,
    pub perms: PermissionState,
    pub questions: QuestionState,
    pub mcp: Arc<McpHub>,
    mcp_ready: Arc<AtomicBool>,
    mcp_scopes: Arc<Mutex<std::collections::HashMap<String, McpScope>>>,
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
        let (mcp, mcp_ready) = start_mcp(&config, &directory);
        let mcp_scopes = Arc::new(Mutex::new(std::collections::HashMap::from([(
            directory,
            (mcp.clone(), mcp_ready.clone()),
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
            mcp,
            mcp_ready,
            mcp_scopes,
            usage: Arc::new(UsageDb::open()),
            always: Arc::new(Mutex::new(Default::default())),
            statuses: Arc::new(Mutex::new(Default::default())),
        })
    }

    pub fn for_directory(&self, directory: &str) -> Result<Self, String> {
        let config = Config::load_for(directory)?;
        let mut scopes = self.mcp_scopes.lock().unwrap();
        let (mcp, ready) = scopes
            .entry(directory.to_string())
            .or_insert_with(|| start_mcp(&config, directory))
            .clone();
        let mut scoped = self.clone();
        scoped.mcp = mcp;
        scoped.mcp_ready = ready;
        scoped.config = Arc::new(Mutex::new(config));
        Ok(scoped)
    }

    pub async fn wait_mcp_ready(&self) {
        while !self.mcp_ready.load(Ordering::Acquire) {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
}

fn start_mcp(config: &Config, directory: &str) -> (Arc<McpHub>, Arc<AtomicBool>) {
    let hub = Arc::new(McpHub::new());
    let ready = Arc::new(AtomicBool::new(config.mcp.is_empty()));
    if config.mcp.is_empty() {
        return (hub, ready);
    }
    let entries = config.mcp.clone();
    let directory = directory.to_string();
    let background = hub.clone();
    let done = ready.clone();
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
    (hub, ready)
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
