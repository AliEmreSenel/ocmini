use super::failure;
use crate::types::{OperationError, ToolOutput};
use serde_json::json;
#[cfg(test)]
use serde_json::Value;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const MAX_BODY: usize = 5 * 1024 * 1024;

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub(super) struct Input {
    pub url: String,
    #[serde(default)]
    format: Format,
    #[schemars(range(min = 1, max = 120))]
    timeout: Option<u64>,
}

#[derive(
    Clone, Copy, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
enum Format {
    Text,
    #[default]
    Markdown,
    Html,
}

fn request_timeout(seconds: Option<u64>) -> Result<Duration, OperationError> {
    let seconds = seconds.unwrap_or(30);
    if seconds == 0 {
        return Err(failure(
            "webfetch timeout must be a positive integer in seconds",
        ));
    }
    Ok(Duration::from_secs(seconds.min(120)))
}

pub(super) async fn fetch(
    input: &Input,
    cancel: CancellationToken,
) -> Result<ToolOutput, OperationError> {
    let url = &input.url;
    let parsed = reqwest::Url::parse(url).map_err(|e| failure(format!("Invalid URL: {e}")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(failure("webfetch accepts HTTP and HTTPS URLs"));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(failure("URL credentials are not supported"));
    }
    let format = input.format;
    let timeout = request_timeout(input.timeout)?;
    let client = crate::provider::client();
    let operation = async {
        let response = client
            .get(parsed)
            .timeout(timeout)
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
        let bytes = crate::http::read_body(response, None, MAX_BODY).await?;
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
        let body = if is_html && format != Format::Html {
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
        _ = cancel.cancelled() => Err(failure("Fetch cancelled")),
    }
}

fn render_html(bytes: &[u8], format: Format) -> Result<String, OperationError> {
    let rendered = if format == Format::Text {
        html2text::from_read_with_decorator(bytes, 120, html2text::render::TrivialDecorator::new())
    } else {
        html2text::from_read(bytes, 120)
    };
    rendered.map_err(|error| failure(format!("Cannot render HTML: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request_timeout(input: &Value) -> Result<Duration, OperationError> {
        super::request_timeout(
            crate::tools::decode::<Input>(&{
                let mut input = input.clone();
                input["url"] = json!("https://example.com");
                input
            })?
            .timeout,
        )
    }
    async fn fetch(input: &Value, cancel: CancellationToken) -> Result<ToolOutput, OperationError> {
        super::fetch(&crate::tools::decode(input)?, cancel).await
    }

    #[test]
    fn web_timeout_defaults_and_ceiling_match_opencode() {
        assert_eq!(
            request_timeout(&json!({})).unwrap(),
            Duration::from_secs(30)
        );
        assert_eq!(
            request_timeout(&json!({"timeout":120})).unwrap(),
            Duration::from_secs(120)
        );
        assert_eq!(
            request_timeout(&json!({"timeout":3600})).unwrap(),
            Duration::from_secs(120)
        );
        assert!(request_timeout(&json!({"timeout":0})).is_err());
    }

    #[test]
    fn html_formats_decode_entities_omit_scripts_and_keep_links() {
        let html = b"<html><script>secretScript()</script><h1>Title &amp; stuff</h1><p>Hello <a href='https://example.com'>world</a>.</p></html>";
        let text = render_html(html, Format::Text).unwrap();
        assert!(text.contains("Title & stuff"));
        assert!(text.contains("Hello world."));
        assert!(!text.contains("secretScript"));
        let markdown = render_html(html, Format::Markdown).unwrap();
        assert!(markdown.contains("https://example.com"));
        assert!(markdown.contains("# Title"));
    }

    #[tokio::test]
    async fn invalid_schemes_and_formats_are_rejected_before_network() {
        assert!(fetch(
            &json!({"url":"file:///etc/passwd"}),
            CancellationToken::new()
        )
        .await
        .is_err());
        assert!(fetch(
            &json!({"url":"http://localhost","format":"binary"}),
            CancellationToken::new()
        )
        .await
        .is_err());
    }
}
