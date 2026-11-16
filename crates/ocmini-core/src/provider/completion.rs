//! Every model request shares cancellation, terminal validation and lifecycle
//! events. Callers consume content and choose their own retry policy.
use super::{ChatRequest, ProviderAdapter, ProviderError};
use crate::engine::App;
use crate::events as event;
use crate::types::{FailureKind, OperationError, ProviderEvent, Tokens};
use futures::StreamExt;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

pub struct TextCompletion {
    pub text: String,
    pub finish: String,
    pub usage: Tokens,
}

pub async fn request_text(
    app: &App,
    adapter: &dyn ProviderAdapter,
    context: &Value,
    request: &ChatRequest,
    cancel: &CancellationToken,
) -> Result<TextCompletion, ProviderError> {
    let mut text = String::new();
    let (finish, usage) = self::request(app, adapter, context, request, cancel, |event| {
        if let ProviderEvent::TextDelta(delta) = event {
            text.push_str(&delta);
        }
        Ok(())
    })
    .await?;
    Ok(TextCompletion {
        text,
        finish,
        usage,
    })
}

pub async fn request(
    app: &App,
    adapter: &dyn ProviderAdapter,
    context: &Value,
    request: &ChatRequest,
    cancel: &CancellationToken,
    mut consume: impl FnMut(ProviderEvent) -> Result<(), OperationError>,
) -> Result<(String, Tokens), ProviderError> {
    app.hooks.emit(
        event::Model::Started {
            attempt: context["attempt"].as_u64(),
        },
        context,
    );
    let result = async {
        let mut stream = tokio::select! {
            result = adapter.stream(request) => result?,
            _ = cancel.cancelled() => return Err(OperationError::new(FailureKind::Cancelled, "Aborted").into()),
        };
        while let Some(event) = tokio::select! {
            event = stream.next() => event,
            _ = cancel.cancelled() => return Err(OperationError::new(FailureKind::Cancelled, "Aborted").into()),
        } {
            match event {
                ProviderEvent::Finished { finish, usage } => return Ok((finish, usage)),
                ProviderEvent::Error(message) => {
                    return Err(ProviderError {
                        retryable: super::stream_retryable(&message),
                        ..ProviderError::fatal(message)
                    })
                }
                event => consume(event).map_err(ProviderError::from)?,
            }
        }
        Err(ProviderError {
            retryable: true,
            ..ProviderError::fatal("provider stream ended before a completion event")
        })
    }
    .await;
    match &result {
        Ok((finish, usage)) => app.hooks.emit(
            event::Model::Completed {
                attempt: context["attempt"].as_u64(),
                finish,
                tokens: usage,
            },
            context,
        ),
        Err(error) => app.hooks.emit(
            event::Model::Failed {
                attempt: context["attempt"].as_u64(),
                error: &error.message,
                retryable: error.retryable,
            },
            context,
        ),
    }
    result
}
