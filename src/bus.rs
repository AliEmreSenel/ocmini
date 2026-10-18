use crate::types::BusEvent;
use std::sync::Arc;
use tokio::sync::broadcast;

#[derive(Clone)]
pub struct Bus {
    tx: broadcast::Sender<Arc<BusEvent>>,
}

impl Bus {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(16384);
        Self { tx }
    }

    pub fn emit(&self, event_type: &str, properties: serde_json::Value) {
        let ev = BusEvent {
            id: crate::types::hex_id("evt_"),
            r#type: event_type.to_string(),
            properties,
        };
        // Subscribers share immutable payloads, including large tool results.
        let _ = self.tx.send(Arc::new(ev));
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<BusEvent>> {
        self.tx.subscribe()
    }
}
