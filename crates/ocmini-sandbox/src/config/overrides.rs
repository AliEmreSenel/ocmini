//! Trusted process-local changes, applied after preset inheritance and before
//! path expansion and validation. They never change preset files.
use serde_json::{json, Value};

#[derive(Debug)]
enum Change {
    Merge(Value),
    Set(String, Value),
    Add(String, Value),
}

#[derive(Debug, Default)]
pub struct Overrides {
    changes: Vec<Change>,
}

impl Overrides {
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    pub fn merge(&mut self, value: Value) -> Result<(), String> {
        let object = value
            .as_object()
            .ok_or("Sandbox overlay must be a JSON object")?;
        for key in object.keys() {
            policy_key(key)?;
        }
        self.changes.push(Change::Merge(value));
        Ok(())
    }

    pub fn set(&mut self, key: &str, value: Value) -> Result<(), String> {
        policy_key(key)?;
        self.changes.push(Change::Set(key.into(), value));
        Ok(())
    }

    pub fn add(&mut self, key: &str, value: Value) -> Result<(), String> {
        policy_key(key)?;
        self.changes.push(Change::Add(key.into(), value));
        Ok(())
    }

    pub(super) fn apply(&self, policy: &mut Value) -> Result<(), String> {
        for change in &self.changes {
            match change {
                Change::Merge(value) => merge(policy, value),
                Change::Set(key, value) => *field(policy, key)? = value.clone(),
                Change::Add(key, value) => {
                    let target = field(policy, key)?;
                    if target.is_null() {
                        *target = json!([]);
                    }
                    let list = target
                        .as_array_mut()
                        .ok_or_else(|| format!("Sandbox override {key} is not a list"))?;
                    for item in value
                        .as_array()
                        .map(Vec::as_slice)
                        .unwrap_or(std::slice::from_ref(value))
                    {
                        if !list.contains(item) {
                            list.push(item.clone());
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

fn policy_key(key: &str) -> Result<(), String> {
    if matches!(key.split('.').next(), Some("off" | "extends")) {
        return Err(
            "Sandbox overrides cannot change off or extends; select a preset with --sandbox-preset"
                .into(),
        );
    }
    if !key.split('.').all(|part| {
        !part.is_empty() && part.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
    }) {
        return Err(format!("Invalid sandbox override key: {key}"));
    }
    Ok(())
}

fn field<'a>(mut value: &'a mut Value, key: &str) -> Result<&'a mut Value, String> {
    for part in key.split('.') {
        if value.is_null() {
            *value = json!({});
        }
        value = value
            .as_object_mut()
            .ok_or_else(|| format!("Sandbox override {key} crosses a non-object value"))?
            .entry(part)
            .or_insert(Value::Null);
    }
    Ok(value)
}

fn merge(target: &mut Value, overlay: &Value) {
    if let (Some(target), Some(overlay)) = (target.as_object_mut(), overlay.as_object()) {
        for (key, value) in overlay {
            merge(target.entry(key).or_insert(Value::Null), value);
        }
    } else {
        *target = overlay.clone();
    }
}
