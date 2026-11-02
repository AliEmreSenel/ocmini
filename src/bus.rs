use crate::events as event;
use crate::types::BusEvent;
use std::sync::{Arc, Mutex, RwLock, Weak};
use tokio::sync::broadcast;

#[derive(Clone)]
pub struct Bus {
    tx: broadcast::Sender<Arc<BusEvent>>,
    v2: Arc<RwLock<Arc<Vec<Weak<crate::v2::Events>>>>>,
    observers: Arc<Mutex<Vec<Weak<crate::hooks::ObserverRegistry>>>>,
}

impl Bus {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(16384);
        Self {
            tx,
            v2: Arc::new(RwLock::new(Arc::new(Vec::new()))),
            observers: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn emit<'a>(&self, event: impl Into<event::Event<'a>>, context: serde_json::Value) {
        self.publish(Arc::new(BusEvent::new(event, context)));
    }

    pub fn register_v2(&self, events: &Arc<crate::v2::Events>) {
        let mut bridges = self.v2.write().unwrap();
        let bridges = Arc::make_mut(&mut bridges);
        bridges.retain(|bridge| bridge.strong_count() > 0);
        bridges.push(Arc::downgrade(events));
    }

    pub fn publish(&self, ev: Arc<BusEvent>) {
        let bridges = self.v2.read().unwrap().clone();
        for bridge in bridges.iter().filter_map(Weak::upgrade) {
            bridge.observe(&ev);
        }
        let observers = {
            let mut observers = self.observers.lock().unwrap();
            observers.retain(|registry| registry.strong_count() > 0);
            observers
                .iter()
                .filter_map(Weak::upgrade)
                .collect::<Vec<_>>()
        };
        for observer in observers {
            observer.observe(&ev);
        }
        let _ = self.tx.send(ev);
    }

    pub fn register_observers(&self, registry: &Arc<crate::hooks::ObserverRegistry>) {
        self.observers
            .lock()
            .unwrap()
            .push(Arc::downgrade(registry));
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
            event::Part::Delta {
                session_id: "ses",
                message_id: "msg",
                part_id: "prt",
                field: event::DeltaField::Text,
                delta: "text",
            },
            serde_json::json!({}),
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
