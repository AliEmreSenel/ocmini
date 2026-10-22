// Session titles from the first prompt: deterministic, model-written, with
// the deterministic form as the safety net.
use crate::config::Config;
use crate::engine::{bus, App};
use crate::provider::{self, ChatRequest};
use crate::sessions;
use crate::types::{ModelRef, ProviderEvent, Session};
use futures::StreamExt;
use serde_json::{json, Value};

/// Ask for a title, not an answer: the framing keeps small models from
/// replying to the request. The first line of the reply becomes the title.
const TITLE_PROMPT: &str = "Name this conversation for a session list. Reply with one short line of at most 80 characters: no quotes, no trailing punctuation, nothing else.";

/// A title request carries the prompt, not the transcript.
const MAX_PROMPT_CHARS: usize = 2000;

/// Models to try, in order: `title_model`, `small_model`, the session's model.
/// An unset `title_model` means deterministic titles only; `small` starts at
/// the small model.
fn chain(cfg: &Config, session_model: &ModelRef) -> Vec<ModelRef> {
    let wanted = match cfg.title_model.trim() {
        "" => return Vec::new(),
        "small" => None,
        spec => ModelRef::parse(spec).ok(),
    };
    let mut chain = wanted.into_iter().collect::<Vec<_>>();
    for model in ModelRef::parse(&cfg.small_model)
        .into_iter()
        .chain([session_model.clone()])
    {
        if !chain.contains(&model) {
            chain.push(model);
        }
    }
    chain
}

/// Name a session after its first prompt. Without `title_model` this renames
/// from the prompt text on the spot; with it, the model writes the title on a
/// background task. Every failure (unavailable model, provider error, useless
/// reply) moves to the next model, and the last resort is the prompt's first
/// line: a session is never left wearing its launcher title.
pub(crate) fn launch(app: App, sid: String, parts: &[Value], session_model: &ModelRef) {
    // Admission calls this for every prompt; naming is a first-prompt job. A
    // session that already has a real title needs no rename, so a later prompt
    // starts neither a title request nor the "generating title…" line that
    // `set_title` would refuse and `apply` would never answer.
    let Ok(Some(session)) = app.store.get_session(&sid) else {
        return;
    };
    if !sessions::is_placeholder(&session.title) {
        return;
    }
    let chain = chain(&app.config.lock().unwrap(), session_model);
    if chain.is_empty() {
        apply(sessions::auto_title(&app.store, &sid, parts), &sid);
        return;
    }
    let prompt: String = sessions::prompt_text(parts)
        .chars()
        .take(MAX_PROMPT_CHARS)
        .collect();
    if prompt.trim().is_empty() {
        return; // An image-only prompt has nothing to name the session after.
    }
    let parts = parts.to_vec();
    tokio::spawn(async move {
        bus().emit(
            "session.title",
            json!({"sessionID": &sid, "state": "start"}),
        );
        let mut generated = None;
        for model in chain {
            let cfg = app.config.lock().unwrap().clone();
            if !crate::engine_impl::model_available(&cfg, &model) {
                eprintln!("ocmini: title model {model} is unavailable, trying the next");
                continue;
            }
            match request(&app, &sid, &cfg, &model, &prompt).await {
                Ok(Some(reply)) => {
                    generated = sessions::title_from_reply(&reply);
                    if generated.is_some() {
                        break;
                    }
                    eprintln!("ocmini: title model {model} returned no usable title");
                }
                Ok(None) => eprintln!("ocmini: title model {model} ended without a reply"),
                Err(error) => eprintln!("ocmini: title request to {model} failed: {error}"),
            }
        }
        // The deterministic title is the safety net: a dead side request
        // cannot leave "tui session" in the resume list.
        apply(
            match generated {
                Some(title) => sessions::set_title(&app.store, &sid, &title),
                None => sessions::auto_title(&app.store, &sid, &parts),
            },
            &sid,
        );
    });
}

fn apply(result: Result<Option<Session>, String>, sid: &str) {
    if let Ok(Some(session)) = result {
        bus().emit(
            "session.title",
            json!({"sessionID": sid, "state": "done", "title": session.title}),
        );
        bus().emit(
            "session.updated",
            json!({"sessionID": sid, "info": session}),
        );
    }
}

/// One tool-free title request. `variant` stays `None` on purpose: the
/// session's variant belongs to its model, and leaking it to the title model
/// is a 400 (upstream's effort-leak failure).
async fn request(
    app: &App,
    sid: &str,
    cfg: &Config,
    model: &ModelRef,
    prompt: &str,
) -> Result<Option<String>, String> {
    let adapter = provider::build(
        &model.provider_id,
        cfg.provider(&model.provider_id),
        &app.auth,
    );
    let chat = ChatRequest {
        model: model.model_id.clone(),
        system: TITLE_PROMPT.into(),
        messages: vec![json!({"role": "user", "content": prompt})],
        tools: Vec::new(),
        variant: None,
        session_id: Some(sid.into()),
    };
    let mut stream = adapter.stream(&chat).await.map_err(|error| error.message)?;
    let mut reply = String::new();
    let mut failed = None;
    while let Some(event) = stream.next().await {
        match event {
            ProviderEvent::TextDelta(delta) => reply.push_str(&delta),
            ProviderEvent::Finished { .. } => break,
            ProviderEvent::Error(error) => {
                failed = Some(error);
                break;
            }
            _ => {}
        }
    }
    drop(stream);
    match failed {
        Some(error) => Err(error),
        None => Ok(Some(reply)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(title_model: &str, small_model: &str) -> Config {
        serde_json::from_value(json!({"title_model": title_model, "small_model": small_model}))
            .unwrap()
    }

    #[test]
    fn unset_title_model_means_deterministic_titles() {
        let session = ModelRef::parse("anthropic/opus").unwrap();
        assert!(chain(&cfg("", ""), &session).is_empty());
    }

    #[test]
    fn chain_orders_title_small_and_session_models() {
        let session = ModelRef::parse("anthropic/opus").unwrap();
        assert_eq!(
            chain(&cfg("openai/gpt-mini", "anthropic/haiku"), &session),
            [
                ModelRef::parse("openai/gpt-mini").unwrap(),
                ModelRef::parse("anthropic/haiku").unwrap(),
                session.clone()
            ]
        );
        // "small" starts at the small model; duplicates appear once, and the
        // session model is always the last stop.
        assert_eq!(
            chain(&cfg("small", "anthropic/haiku"), &session),
            [ModelRef::parse("anthropic/haiku").unwrap(), session.clone()]
        );
        assert_eq!(
            chain(&cfg("anthropic/opus", ""), &session),
            std::slice::from_ref(&session)
        );
        // A malformed model name falls through to the rest of the chain.
        assert_eq!(
            chain(&cfg("broken", "anthropic/haiku"), &session),
            [ModelRef::parse("anthropic/haiku").unwrap(), session.clone()]
        );
    }
}
