//! HTTP/SSE handling is shared; each codec owns its payload and terminal rules.
use super::{ChatRequest, EventStream, Protocol, ProviderError, Settings};
use crate::types::ProviderEvent;
use futures::StreamExt;
use serde_json::{json, Value};

pub(super) trait Decoder: Default + Send + 'static {
    const PROTOCOL: Protocol;
    fn consume(&mut self, value: &Value) -> Result<Vec<ProviderEvent>, String>;
    fn json(&mut self, value: Value) -> Result<Vec<ProviderEvent>, String>;
    fn terminal(&self) -> bool;
    fn end(&mut self) {}
    fn finish(self, model: &str) -> Result<Vec<ProviderEvent>, String>;
}

pub(super) async fn request<D: Decoder>(
    settings: Settings,
    req: &ChatRequest,
    endpoint: &str,
    body: fn(&Settings, &ChatRequest) -> Result<Value, String>,
) -> Result<EventStream, ProviderError> {
    let response = settings
        .send(
            endpoint,
            D::PROTOCOL,
            &body(&settings, req).map_err(ProviderError::fatal)?,
        )
        .await?;
    let stream = async_stream::try_stream! {
        let mut state = D::default();
        if super::json_response(&response) {
            let bytes = crate::http::read_body(response, Some(settings.chunk_timeout), 16 * 1024 * 1024).await?;
            let value = serde_json::from_slice(&bytes)
                .map_err(|error| format!("invalid provider JSON response: {error}"))?;
            for event in state.json(value)? { yield event; }
        } else {
            let mut frames = super::sse::frames(response, Some(settings.chunk_timeout));
            while let Some(frame) = frames.next().await {
                let frame = frame?;
                if frame.data.trim().is_empty() { continue; }
                if frame.data.trim() == "[DONE]" { state.end(); break; }
                let mut value: Value = serde_json::from_str(&frame.data)
                    .map_err(|error| format!("invalid provider SSE JSON: {error}"))?;
                if value.get("type").is_none() { value["type"] = json!(frame.event); }
                for event in state.consume(&value)? { yield event; }
                if state.terminal() { break; }
            }
        }
        for event in state.finish(&settings.api_model)? { yield event; }
    };
    Ok(Box::new(Box::pin(stream.map(
        |event: Result<_, String>| event.unwrap_or_else(ProviderEvent::Error),
    ))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{anthropic, openai_compat, responses, Protocol};
    use axum::{routing::post, Router};

    async fn decode(
        protocol: Protocol,
        body: String,
        content_type: &'static str,
    ) -> Vec<ProviderEvent> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut settings = crate::provider::test_settings(json!({}));
        settings.base_url = format!("http://{}", listener.local_addr().unwrap());
        let router = Router::new().route(
            "/{*path}",
            post(move || {
                let body = body.clone();
                async move { ([("content-type", content_type)], body) }
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let request = ChatRequest::default();
        let stream = match protocol {
            Protocol::Chat => openai_compat::stream(settings, &request).await,
            Protocol::Anthropic => anthropic::stream(settings, &request).await,
            Protocol::Responses => responses::stream(settings, &request).await,
        }
        .unwrap();
        let events = stream.collect().await;
        server.abort();
        events
    }

    #[tokio::test]
    async fn all_codecs_decode_http_json_and_sse_with_one_terminal_and_usage() {
        let chat = json!({"choices":[{"message":{"content":"answer"},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":1}});
        let anthropic = json!({"content":[{"type":"text","text":"answer"}],"stop_reason":"end_turn","usage":{"input_tokens":2,"output_tokens":1}});
        let responses = json!({"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]}],"usage":{"input_tokens":2,"output_tokens":1}});
        for (protocol, json, events) in [
            (Protocol::Chat, chat.clone(), vec![chat]),
            (
                Protocol::Anthropic,
                anthropic.clone(),
                vec![
                    json!({"type":"message_start","message":{"usage":anthropic["usage"]}}),
                    json!({"type":"content_block_start","index":0,"content_block":anthropic["content"][0]}),
                    json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}),
                    json!({"type":"message_stop"}),
                ],
            ),
            (
                Protocol::Responses,
                responses.clone(),
                vec![json!({"type":"response.completed","response":responses})],
            ),
        ] {
            let sse = events
                .iter()
                .map(|event| format!("data: {event}\n\n"))
                .collect::<String>();
            for (body, content_type) in [
                (json.to_string(), "application/json"),
                (sse, "text/event-stream"),
            ] {
                let events = decode(protocol, body, content_type).await;
                let text: String = events
                    .iter()
                    .filter_map(|event| match event {
                        ProviderEvent::TextDelta(text) => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                assert_eq!(text, "answer", "{protocol:?}");
                assert!(!events
                    .iter()
                    .any(|event| matches!(event, ProviderEvent::Error(_))));
                let usage: Vec<_> = events
                    .iter()
                    .filter_map(|event| match event {
                        ProviderEvent::Finished { usage, .. } => Some(usage),
                        _ => None,
                    })
                    .collect();
                assert_eq!(usage.len(), 1);
                assert_eq!((usage[0].input, usage[0].output), (2, 1));
            }
        }
    }

    #[tokio::test]
    async fn incomplete_terminal_payloads_never_become_successful_completions() {
        for (protocol, body, content_type) in [
            (
                Protocol::Chat,
                r#"{"choices":[{"message":{"content":"partial"}}]}"#,
                "application/json",
            ),
            (Protocol::Chat, "data: [DONE]\n\n", "text/event-stream"),
            (
                Protocol::Anthropic,
                "data: {\"type\":\"message_stop\"}\n\n",
                "text/event-stream",
            ),
            (
                Protocol::Responses,
                "data: {\"type\":\"response.completed\"}\n\n",
                "text/event-stream",
            ),
        ] {
            let events = decode(protocol, body.into(), content_type).await;
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, ProviderEvent::Error(_))),
                "{protocol:?}"
            );
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, ProviderEvent::Finished { .. })),
                "{protocol:?}"
            );
        }
    }
}
