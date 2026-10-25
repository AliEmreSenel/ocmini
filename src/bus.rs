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
        self.publish(Arc::new(ev));
    }

    pub fn publish(&self, ev: Arc<BusEvent>) {
        if crate::hooks::has_observers() {
            crate::hooks::observe(&ev);
        }
        let _ = self.tx.send(ev);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<BusEvent>> {
        self.tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn subscribers_share_one_payload_and_keep_the_sse_shape() {
        let bus = Bus::new();
        let mut a = bus.subscribe();
        let mut b = bus.subscribe();
        bus.emit(
            "message.part.delta",
            serde_json::json!({"sessionID":"ses","delta":"text"}),
        );
        let first = a.recv().await.unwrap();
        let second = b.recv().await.unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        let value = serde_json::to_value(first.as_ref()).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 3);
        assert_eq!(value["type"], "message.part.delta");
        assert_eq!(value["properties"]["delta"], "text");
    }
}
