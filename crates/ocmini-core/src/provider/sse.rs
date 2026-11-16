use async_stream::stream;
use futures::{Stream, StreamExt};
use std::pin::Pin;
use std::time::Duration;

pub struct Frame {
    pub event: String,
    pub data: String,
}

#[derive(Default)]
pub struct Decoder {
    line: Vec<u8>,
    data: Vec<String>,
    event: String,
    skip_lf: bool,
    first_line: bool,
    size: usize,
}

impl Decoder {
    pub fn new() -> Self {
        Self {
            first_line: true,
            ..Self::default()
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<Frame>, String> {
        let mut frames = Vec::new();
        for byte in bytes {
            if self.skip_lf {
                self.skip_lf = false;
                if *byte == b'\n' {
                    continue;
                }
            }
            if *byte == b'\r' || *byte == b'\n' {
                self.skip_lf = *byte == b'\r';
                if let Some(frame) = self.finish_line()? {
                    frames.push(frame);
                }
            } else {
                self.line.push(*byte);
                if self.line.len() + self.size > 16 * 1024 * 1024 {
                    return Err("provider SSE frame exceeds 16 MiB".into());
                }
            }
        }
        Ok(frames)
    }

    fn finish_line(&mut self) -> Result<Option<Frame>, String> {
        let bytes = std::mem::take(&mut self.line);
        let result = self.parse_line(&bytes);
        // Reuse typical event buffers without retaining a large outlier for
        // the rest of a long-lived stream.
        if bytes.capacity() <= 64 * 1024 {
            self.line = bytes;
            self.line.clear();
        }
        result
    }

    fn parse_line(&mut self, bytes: &[u8]) -> Result<Option<Frame>, String> {
        let line =
            std::str::from_utf8(bytes).map_err(|_| "provider SSE stream contains invalid UTF-8")?;
        let line = if self.first_line {
            self.first_line = false;
            line.trim_start_matches('\u{feff}')
        } else {
            line
        };
        if line.is_empty() {
            return Ok(self.take_frame());
        }
        if line.starts_with(':') {
            return Ok(None);
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "data" => {
                self.size += value.len() + 1;
                self.data.push(value.to_string());
            }
            "event" => self.event = value.to_string(),
            _ => {}
        }
        Ok(None)
    }

    fn take_frame(&mut self) -> Option<Frame> {
        self.size = 0;
        if self.data.is_empty() {
            self.event.clear();
            return None;
        }
        Some(Frame {
            event: std::mem::take(&mut self.event),
            data: std::mem::take(&mut self.data).join("\n"),
        })
    }

    pub fn finish(&mut self) -> Result<Vec<Frame>, String> {
        let mut frames = Vec::new();
        if !self.line.is_empty() {
            if let Some(frame) = self.finish_line()? {
                frames.push(frame);
            }
        }
        if let Some(frame) = self.take_frame() {
            frames.push(frame);
        }
        Ok(frames)
    }
}

pub fn frames(
    response: reqwest::Response,
    timeout: Option<Duration>,
) -> Pin<Box<dyn Stream<Item = Result<Frame, String>> + Send>> {
    Box::pin(stream! {
        let mut bytes = response.bytes_stream();
        let mut decoder = Decoder::new();
        loop {
            let chunk = match timeout {
                Some(timeout) => match tokio::time::timeout(timeout, bytes.next()).await {
                    Ok(chunk) => chunk,
                    Err(_) => { yield Err(format!("provider stream timed out waiting for a chunk after {} seconds", timeout.as_secs())); return; }
                },
                None => bytes.next().await,
            };
            match chunk {
                Some(Ok(chunk)) => match decoder.feed(&chunk) {
                    Ok(frames) => { for frame in frames { yield Ok(frame); } }
                    Err(error) => { yield Err(error); return; }
                },
                Some(Err(error)) => { yield Err(format!("provider stream failed: {}", error.without_url())); return; }
                None => {
                    match decoder.finish() {
                        Ok(frames) => { for frame in frames { yield Ok(frame); } }
                        Err(error) => yield Err(error),
                    }
                    return;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preserves_split_utf8_multiline_and_all_line_endings() {
        let source = "\u{feff}: comment\r\nevent: item\r\ndata: {\r\ndata: \"text\": \"café 中文\"}\r\n\r\ndata: [DONE]\r\r";
        for split in 0..=source.len() {
            let mut decoder = Decoder::new();
            let mut frames = decoder.feed(&source.as_bytes()[..split]).unwrap();
            frames.extend(decoder.feed(&source.as_bytes()[split..]).unwrap());
            frames.extend(decoder.finish().unwrap());
            assert_eq!(frames.len(), 2);
            assert_eq!(frames[0].event, "item");
            assert_eq!(frames[0].data, "{\n\"text\": \"café 中文\"}");
            assert_eq!(frames[1].data, "[DONE]");
        }
    }
    #[test]
    fn flushes_final_data_and_rejects_malformed_utf8() {
        let mut decoder = Decoder::new();
        assert!(decoder.feed(b"data: eof").unwrap().is_empty());
        assert_eq!(decoder.finish().unwrap()[0].data, "eof");
        assert!(decoder.feed(b"data: \xff\n\n").is_err());
    }
}
