use crate::types::BusEvent;
use tokio::sync::broadcast;

#[derive(Clone)]
pub struct Bus {
    tx: broadcast::Sender<BusEvent>,
}

impl Bus {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(4096);
        Self { tx }
    }

    pub fn emit(&self, event_type: &str, properties: serde_json::Value) {
        let ev = BusEvent {
            id: crate::types::hex_id("evt_"),
            r#type: event_type.to_string(),
            properties,
        };
        let _ = self.tx.send(ev);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<BusEvent> {
        self.tx.subscribe()
    }
}
