//! Bounded response reads shared by providers, MCP, instructions and webfetch.
use std::time::Duration;

pub async fn read_body(
    mut response: reqwest::Response,
    timeout: Option<Duration>,
    limit: usize,
) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    loop {
        let read = response.chunk();
        let chunk = match timeout {
            Some(timeout) => tokio::time::timeout(timeout, read)
                .await
                .map_err(|_| "response body timed out")?,
            None => read.await,
        }
        .map_err(|error| error.without_url().to_string())?;
        let Some(chunk) = chunk else { return Ok(bytes) };
        if chunk.len() > limit.saturating_sub(bytes.len()) {
            return Err(format!("response exceeds {limit} bytes"));
        }
        bytes.extend_from_slice(&chunk);
    }
}

pub fn percent_bytes(value: &str) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    let mut input = value.as_bytes().iter().copied();
    while let Some(byte) = input.next() {
        if byte != b'%' {
            bytes.push(byte);
            continue;
        }
        let a = input
            .next()
            .and_then(|b| (b as char).to_digit(16))
            .ok_or("Invalid percent escape")?;
        let b = input
            .next()
            .and_then(|b| (b as char).to_digit(16))
            .ok_or("Invalid percent escape")?;
        bytes.push((a * 16 + b) as u8);
    }
    Ok(bytes)
}
