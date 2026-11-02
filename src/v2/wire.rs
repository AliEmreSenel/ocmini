use crate::types::*;
use serde_json::{json, Value};

pub fn model_in(value: &Value) -> Result<ModelRef, OperationError> {
    let model = ModelRef {
        provider_id: string(value, "providerID")?.into(),
        model_id: string(value, "id")?.into(),
    };
    model.validate().map_err(OperationError::invalid)?;
    Ok(model)
}
pub fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str, OperationError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| OperationError::invalid(format!("{key} must be a string")))
}
pub fn rules(value: &Value) -> Result<Vec<PermissionRule>, OperationError> {
    let list = value
        .as_array()
        .ok_or_else(|| OperationError::invalid("permissions must be an array"))?;
    list.iter()
        .map(|r| {
            let name = string(r, "action")?;
            Ok(PermissionRule {
                permission: match name {
                    "shell" => "bash",
                    "subagent" => "task",
                    other => other,
                }
                .into(),
                pattern: string(r, "resource")?.into(),
                action: serde_json::from_value(r["effect"].clone())
                    .map_err(|_| OperationError::invalid("Invalid permission effect"))?,
            })
        })
        .collect()
}
pub fn rules_out(rules: &[PermissionRule]) -> Value {
    json!(rules
        .iter()
        .map(|r| json!({"action":tool_name(&r.permission),"resource":r.pattern,"effect":r.action}))
        .collect::<Vec<_>>())
}
pub fn tool_name(name: &str) -> &str {
    match name {
        "bash" => "shell",
        "task" => "subagent",
        other => other,
    }
}
pub fn error(message: &str) -> Value {
    json!({"type":"unknown","message":message})
}
pub fn session(s: &Session, state: &Value) -> Value {
    let mut value = json!({"id":s.id,"projectID":s.project_id,"title":s.title,"cost":s.cost,"tokens":s.tokens,"location":{"directory":s.directory},"time":{"created":s.time.created,"updated":s.time.updated.unwrap_or(s.time.created)}});
    for key in ["agent", "model", "outcome", "revert", "fork"] {
        if let Some(v) = state.get(key).filter(|v| !v.is_null()) {
            value[key] = v.clone();
        }
    }
    if let Some(archived) = s.time.extra.get("archived") {
        value["time"]["archived"] = archived.clone();
    }
    for key in ["busy", "idle"] {
        if let Some(v) = state.get(key).filter(|v| !v.is_null()) {
            value["time"][key] = v.clone();
        }
    }
    if let Some(parent) = &s.parent_id {
        value["parentID"] = json!(parent);
    }
    if let Some(rules) = &s.permission {
        value["permissions"] = rules_out(rules);
    }
    if !s.extra.is_empty() {
        value["metadata"] = json!(s.extra);
    }
    value
}
pub fn inbox(sid: &str, request: &PromptRequest, created: i64) -> Value {
    let input = request.v2.clone().unwrap_or_default();
    let mut payload =
        if input.payload.is_object() && input.payload.as_object().is_some_and(|p| !p.is_empty()) {
            input.payload
        } else {
            json!({"text":text_parts(&request.parts).collect::<Vec<_>>().join("\n")})
        };
    let files = files(&request.parts);
    if !files.is_empty() {
        payload["files"] = json!(files);
    }
    json!({"id":request.message_id,"sessionID":sid,"time":{"created":created},"type":input.kind,"payload":payload,"delivery":input.delivery})
}
pub fn receipt(sid: &str, message: &MessageWithParts) -> Result<Value, OperationError> {
    let input = message
        .info
        .extra
        .get("ocminiV2Input")
        .cloned()
        .map(serde_json::from_value::<super::Input>)
        .transpose()
        .map_err(|e| OperationError::from(e.to_string()))?;
    Ok(inbox(
        sid,
        &PromptRequest {
            message_id: Some(message.info.id.clone()),
            parts: message.parts.clone(),
            v2: input,
            ..Default::default()
        },
        message.info.time.created,
    ))
}
pub fn files(parts: &[Value]) -> Vec<Value> {
    parts.iter().filter(|p|p["type"]=="file").filter_map(|p|{
        let url=p["url"].as_str()?;let (_,data)=url.strip_prefix("data:")?.split_once(',')?;
        let source=p["ocminiSourceURL"].as_str().map(|uri|json!({"type":"uri","uri":uri})).unwrap_or_else(||json!({"type":"inline"}));
        Some(json!({"data":data,"mime":p["mime"],"source":source,"name":p["filename"].as_str().unwrap_or("attachment")}))
    }).collect()
}
pub fn content(part: &Value) -> Option<Value> {
    match part["type"].as_str()? {
        "text" => Some(json!({"type":"text","text":part["text"]})),
        "reasoning" => Some(json!({"type":"reasoning","text":part["text"]})),
        "tool" => {
            let state = &part["state"];
            let mut value = json!({"type":"tool","id":part["callID"],"name":tool_name(part["tool"].as_str().unwrap_or_default()),"executed":false,"time":{"created":state["time"]["start"].as_i64().unwrap_or_else(now_ms)},"state":{}});
            let mut metadata = state["metadata"].clone();
            if !metadata.is_object() {
                metadata = json!({});
            }
            if metadata["background"] == true {
                metadata["status"] = json!("running");
            }
            if let Some(sid) = metadata.get("sessionId").cloned() {
                metadata["sessionID"] = sid;
            }
            if let Some(m) = metadata
                .get("model")
                .cloned()
                .filter(|m| m["modelID"].is_string())
            {
                metadata["model"] = json!({"id":m["modelID"],"providerID":m["providerID"]});
            }
            value["state"] = match state["status"].as_str().unwrap_or("pending") {
                "pending" => {
                    json!({"status":"streaming","input":state["raw"].as_str().unwrap_or("{}")})
                }
                "running" => json!({"status":"running","input":state["input"],"metadata":metadata}),
                "completed" => {
                    json!({"status":"completed","input":state["input"],"content":[{"type":"text","text":state["output"].as_str().unwrap_or_default()}],"metadata":metadata})
                }
                _ => {
                    json!({"status":"error","input":state["input"],"error":error(state["error"].as_str().unwrap_or("Tool failed")),"metadata":metadata})
                }
            };
            if let Some(end) = state["time"]["end"].as_i64() {
                value["time"]["completed"] = json!(end);
            }
            Some(value)
        }
        _ => None,
    }
}
pub fn message(m: &MessageWithParts) -> Option<Value> {
    let i = &m.info;
    if i.extra.get("ocminiV2Cancelled") == Some(&Value::Bool(true)) {
        return None;
    }
    if let Some(value) = i.extra.get("ocminiV2") {
        return Some(value.clone());
    }
    let text = text_parts(&m.parts).collect::<Vec<_>>().join("\n");
    if i.extra
        .get("command")
        .is_some_and(|v| v == "__ocmini_compact")
    {
        return None;
    }
    let mut value = json!({"id":i.id,"time":{"created":i.time.created}});
    if i.summary == Some(true) || i.agent.as_deref() == Some("compaction") {
        value["type"] = json!("compaction");
        value["status"] = json!(if i.summary == Some(true) {
            "completed"
        } else if i.error.is_some() {
            "failed"
        } else {
            "running"
        });
        value["reason"] = i
            .extra
            .get("ocminiV2CompactionReason")
            .cloned()
            .unwrap_or_else(|| json!("auto"));
        if let Some(error) = &i.error {
            value["error"] = self::error(
                error["data"]["message"]
                    .as_str()
                    .unwrap_or("Compaction failed"),
            );
        } else {
            value["summary"] = json!(text);
        }
    } else if i.role == "user" {
        let input = i.extra.get("ocminiV2Input");
        value["type"] = json!(input.and_then(|v| v["kind"].as_str()).unwrap_or("user"));
        value["text"] = json!(text);
        let files = files(&m.parts);
        if !files.is_empty() {
            value["files"] = json!(files);
        }
        if let Some(metadata) = input.and_then(|v| v["payload"].get("metadata")) {
            value["metadata"] = metadata.clone();
        }
    } else if i.role == "assistant" {
        value["type"] = json!("assistant");
        value["agent"] = json!(i.agent.as_deref().unwrap_or("build"));
        value["model"] = json!({"id":i.model_id,"providerID":i.provider_id});
        if let Some(v) = &i.variant {
            value["model"]["variant"] = json!(v);
        }
        value["content"] = json!(m.parts.iter().filter_map(content).collect::<Vec<_>>());
        if let Some(v) = &i.finish {
            value["finish"] = json!(v);
        }
        if let Some(v) = &i.error {
            value["error"] = error(v["data"]["message"].as_str().unwrap_or("Execution failed"));
        }
    } else {
        return None;
    }
    if let Some(end) = i.time.completed {
        value["time"]["completed"] = json!(end);
    }
    if let Some(v) = &i.tokens {
        value["tokens"] = json!(v);
    }
    if let Some(v) = i.cost {
        value["cost"] = json!(v);
    }
    if let Some(native) = i.extra.get("providerMetadata") {
        value["native"] = native.clone();
    }
    Some(value)
}
pub fn permission(p: &PermissionRequest) -> Value {
    let mut value = json!({"id":p.id,"sessionID":p.session_id,"action":tool_name(&p.permission),"resources":p.patterns,"save":p.always,"metadata":p.metadata});
    if let Some(tool) = &p.tool {
        value["source"] = json!({"type":"tool","messageID":tool["messageID"],"id":tool["callID"]});
    }
    value
}
pub fn form(p: &QuestionRequest) -> Value {
    let fields:Vec<_>=p.questions.iter().enumerate().map(|(n,q)|{
        let options:Vec<_>=q["options"].as_array().into_iter().flatten().map(|o| {
            let mut option = json!({"value":o["label"],"label":o["label"]});
            if let Some(description) = o.get("description").filter(|v| v.is_string()) { option["description"] = description.clone(); }
            option
        }).collect();
        let mut field=json!({"key":n.to_string(),"type":if q["multiple"]==true {"multiselect"}else{"string"},"required":true,"options":options});
        for (key, source) in [("title", "header"), ("description", "question")] {
            if let Some(value) = q.get(source).filter(|v| v.is_string()) { field[key] = value.clone(); }
        }
        if q["multiple"]!=true {field["custom"]=json!(q["custom"].as_bool().unwrap_or(true));}
        field
    }).collect();
    json!({"id":format!("frm_{}",p.id.trim_start_matches("que_")),"sessionID":p.session_id,"title":"Question","fields":fields})
}
