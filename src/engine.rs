use crate::auth::AuthStore;
use crate::bus::Bus;
use crate::config::Config;
use crate::mcp::McpHub;
use crate::permission::PermissionState;
use self::question::QuestionState;
use crate::store::Store;
use crate::usage_db::UsageDb;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::oneshot;

pub mod question {
    use crate::types::QuestionRequest;
    use std::sync::{Arc, Mutex};
    use tokio::sync::oneshot;

    #[derive(Clone, Default)]
    pub struct QuestionState {
        pending: Arc<Mutex<Vec<(QuestionRequest, oneshot::Sender<Option<Vec<Vec<String>>>>)>>>,
    }

    impl QuestionState {
        pub fn ask(
            &self,
            session_id: &str,
            questions: serde_json::Value,
        ) -> (QuestionRequest, oneshot::Receiver<Option<Vec<Vec<String>>>>) {
            let (tx, rx) = oneshot::channel();
            let req = QuestionRequest {
                id: crate::types::hex_id("que_"),
                session_id: session_id.to_string(),
                questions: vec![questions],
            };
            self.pending.lock().unwrap().push((req.clone(), tx));
            (req, rx)
        }

        pub fn reply(&self, id: &str, answers: Vec<Vec<String>>) -> bool {
            self.finish(id, Some(answers), "question.replied")
        }

        pub fn reject(&self, id: &str) -> bool {
            self.finish(id, None, "question.rejected")
        }

        fn finish(&self, id: &str, answers: Option<Vec<Vec<String>>>, ev: &str) -> bool {
            let mut p = self.pending.lock().unwrap();
            if let Some(i) = p.iter().position(|(r, _)| r.id == id) {
                let (req, tx) = p.remove(i);
                let _ = tx.send(answers.clone());
                crate::engine::GLOBAL_BUS.emit(
                    ev,
                    serde_json::json!({"sessionID": req.session_id, "requestID": req.id, "answers": answers}),
                );
                return true;
            }
            false
        }

        pub fn list(&self) -> Vec<QuestionRequest> {
            self.pending.lock().unwrap().iter().map(|(r, _)| r.clone()).collect()
        }
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
    pub usage: Arc<UsageDb>,
    pub cancels: std::sync::Arc<
        std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<std::sync::atomic::AtomicBool>>>,
    >,
}

impl App {
    pub fn new() -> Self {
        let config = Config::load();
        let mcp = std::sync::Arc::new(McpHub::new());
        for (name, m) in &config.mcp {
            if m.enabled == Some(false) {
                continue;
            }
            let cfg = if m.r#type == "remote" || !m.url.is_empty() {
                serde_json::json!({"type": "remote", "url": m.url, "headers": m.headers})
            } else {
                serde_json::json!({"type": "local", "command": m.command})
            };
            let _ = mcp.add(name, cfg);
        }
        Self {
            store: Arc::new(Store::open(&crate::config::store_path())),
            config: Arc::new(Mutex::new(config)),
            auth: Arc::new(AuthStore::load()),
            perms: PermissionState::default(),
            questions: QuestionState::default(),
            mcp,
            usage: Arc::new(UsageDb::open()),
            cancels: std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        }
    }
}

pub trait EngineApi: Send + Sync + 'static {
    fn start_turn(
        &self,
        app: App,
        session_id: String,
        model_id: String,
        provider_id: String,
        text: String,
    ) -> oneshot::Receiver<serde_json::Value>;

    fn abort(&self, session_id: &str);

    fn run_sync(
        &self,
        app: App,
        session_id: String,
        model_id: String,
        provider_id: String,
        text: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<serde_json::Value, String>> + Send>>;

    fn summarize(&self, app: App, session_id: String) -> Result<(), String>;
}

pub fn bus() -> &'static Bus {
    static BUS: OnceLock<Bus> = OnceLock::new();
    BUS.get_or_init(Bus::new)
}

pub static GLOBAL_BUS: LazyBus = LazyBus;

pub struct LazyBus;

impl std::ops::Deref for LazyBus {
    type Target = Bus;
    fn deref(&self) -> &Bus {
        bus()
    }
}
