use super::{cancelled, failure};
use crate::types::{ToolError, ToolOutput};
use futures::StreamExt;
use serde_json::{json, Value};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

const MAX_BODY: usize = 5 * 1024 * 1024;

pub(super) async fn fetch(input: &Value, cancel: Arc<AtomicBool>) -> Result<ToolOutput, ToolError> {
    let url = super::required(input, "url")?;
    let parsed = reqwest::Url::parse(url).map_err(|e| failure(format!("Invalid URL: {e}")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(failure("webfetch accepts HTTP and HTTPS URLs"));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(failure("URL credentials are not supported"));
    }
    let format = input["format"].as_str().unwrap_or("markdown");
    if !matches!(format, "text" | "markdown" | "html") {
        return Err(failure("format must be text, markdown, or html"));
    }
    let timeout = match input.get("timeout") {
        None | Some(Value::Null) => 30,
        Some(value) => value
            .as_u64()
            .filter(|v| *v > 0 && *v <= 120)
            .ok_or_else(|| failure("webfetch timeout must be 1..120 seconds"))?,
    };
    let client = crate::provider::client();
    let operation = async {
        let response = client
            .get(parsed)
            .timeout(Duration::from_secs(timeout))
            .header(
                reqwest::header::ACCEPT,
                "text/markdown, text/html, text/plain, application/json;q=0.9, */*;q=0.1",
            )
            .send()
            .await
            .map_err(|e| failure(format!("Fetch failed: {}", e.without_url())))?;
        let status = response.status();
        if !status.is_success() {
            return Err(failure(format!(
                "HTTP {} {}",
                status.as_u16(),
                status.canonical_reason().unwrap_or("request failed")
            )));
        }
        if response
            .content_length()
            .is_some_and(|size| size > MAX_BODY as u64)
        {
            return Err(failure("Response exceeds the 5 MiB fetch limit"));
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("text/plain")
            .to_owned();
        let final_url = response.url().to_string();
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk =
                chunk.map_err(|e| failure(format!("Response body failed: {}", e.without_url())))?;
            if bytes.len() + chunk.len() > MAX_BODY {
                return Err(failure("Response exceeds the 5 MiB fetch limit"));
            }
            bytes.extend_from_slice(&chunk);
        }
        if bytes.contains(&0)
            || !(content_type.starts_with("text/")
                || content_type.contains("json")
                || content_type.contains("xml")
                || content_type.contains("javascript")
                || content_type.contains("x-www-form-urlencoded"))
        {
            return Err(failure(format!(
                "Response is {content_type}; webfetch returns text resources only"
            )));
        }
        let is_html =
            content_type.contains("text/html") || content_type.contains("application/xhtml+xml");
        let body = if is_html && format != "html" {
            render_html(&bytes, format)?
        } else {
            String::from_utf8(bytes).map_err(|_| failure("Response is not UTF-8 text"))?
        };
        Ok(ToolOutput {
            title: final_url.clone(),
            output: body,
            metadata: json!({"url":final_url,"contentType":content_type,"format":format,"status":status.as_u16()}),
        })
    };
    tokio::select! {
        result = operation => result,
        _ = cancelled(cancel) => Err(failure("Fetch cancelled")),
    }
}

fn render_html(bytes: &[u8], format: &str) -> Result<String, ToolError> {
    let rendered = if format == "text" {
        html2text::from_read_with_decorator(bytes, 120, html2text::render::TrivialDecorator::new())
    } else {
        html2text::from_read(bytes, 120)
    };
    rendered.map_err(|error| failure(format!("Cannot render HTML: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_formats_decode_entities_omit_scripts_and_keep_links() {
        let html = b"<html><script>secretScript()</script><h1>Title &amp; stuff</h1><p>Hello <a href='https://example.com'>world</a>.</p></html>";
        let text = render_html(html, "text").unwrap();
        assert!(text.contains("Title & stuff"));
        assert!(text.contains("Hello world."));
        assert!(!text.contains("secretScript"));
        let markdown = render_html(html, "markdown").unwrap();
        assert!(markdown.contains("https://example.com"));
        assert!(markdown.contains("# Title"));
    }

    #[tokio::test]
    async fn invalid_schemes_and_formats_are_rejected_before_network() {
        assert!(fetch(
            &json!({"url":"file:///etc/passwd"}),
            Arc::new(AtomicBool::new(false))
        )
        .await
        .is_err());
        assert!(fetch(
            &json!({"url":"http://localhost","format":"binary"}),
            Arc::new(AtomicBool::new(false))
        )
        .await
        .is_err());
    }
}
