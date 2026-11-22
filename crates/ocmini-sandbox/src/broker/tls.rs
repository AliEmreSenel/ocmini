//! Incremental ClientHello inspection, including handshakes split over records.
//! Sizes are TLS wire lengths, never arbitrary parser truncation limits.
use crate::config::normalize_name;
/// Maximum wire size of a valid TLS ClientHello body under RFC 5246 / RFC 8446:
/// 2 (version) + 32 (random) + 1 + 255 (session_id) + 2 + 65535 (cipher_suites)
/// + 1 + 255 (compression_methods) + 2 + 65535 (extensions).
const MAX_CLIENT_HELLO_BODY: usize = 34 + 1 + 255 + 2 + 65535 + 1 + 255 + 2 + 65535;
#[derive(Debug, PartialEq, Eq)]
pub struct Hello {
    pub name: Option<String>,
    pub ech: bool,
}
#[derive(Debug, PartialEq, Eq)]
pub enum Parse {
    More,
    Hello(Hello),
    Invalid,
}
#[derive(Default)]
pub struct Inspector {
    header: [u8; 5],
    header_len: usize,
    record_rem: usize,
    handshake: Vec<u8>,
    expected_len: Option<usize>,
    done: bool,
}
impl Inspector {
    pub fn feed(&mut self, mut input: &[u8]) -> Parse {
        if self.done {
            return Parse::Invalid;
        }
        while !input.is_empty() {
            if self.record_rem == 0 {
                let need = 5 - self.header_len;
                let take = input.len().min(need);
                self.header[self.header_len..self.header_len + take]
                    .copy_from_slice(&input[..take]);
                self.header_len += take;
                input = &input[take..];
                if self.header_len < 5 {
                    return Parse::More;
                }
                if self.header[0] != 22 || self.header[1] != 3 {
                    self.done = true;
                    return Parse::Invalid;
                }
                let length = u16::from_be_bytes([self.header[3], self.header[4]]) as usize;
                if length == 0 || length > 18432 {
                    self.done = true;
                    return Parse::Invalid;
                }
                self.record_rem = length;
                self.header_len = 0;
            }
            let take = input.len().min(self.record_rem);
            if let Some(expected) = self.expected_len {
                let want = expected.saturating_sub(self.handshake.len());
                self.handshake.extend_from_slice(&input[..take.min(want)]);
            } else {
                self.handshake.extend_from_slice(&input[..take]);
            }
            self.record_rem -= take;
            input = &input[take..];
            if self.expected_len.is_none() && self.handshake.len() >= 4 {
                if self.handshake[0] != 1 {
                    self.done = true;
                    return Parse::Invalid;
                }
                let body_len = ((self.handshake[1] as usize) << 16)
                    | ((self.handshake[2] as usize) << 8)
                    | self.handshake[3] as usize;
                if body_len > MAX_CLIENT_HELLO_BODY {
                    self.done = true;
                    return Parse::Invalid;
                }
                self.expected_len = Some(4 + body_len);
                if self.handshake.len() > 4 + body_len {
                    self.handshake.truncate(4 + body_len);
                }
            }
            if self.record_rem == 0 {
                if let Some(expected) = self.expected_len {
                    if self.handshake.len() >= expected {
                        self.done = true;
                        return parse_hello(&self.handshake[4..expected])
                            .map_or(Parse::Invalid, Parse::Hello);
                    }
                }
            }
        }
        Parse::More
    }
}
#[cfg(test)]
pub fn inspect(bytes: &[u8]) -> Parse {
    Inspector::default().feed(bytes)
}
fn take<'a>(input: &mut &'a [u8], length: usize) -> Option<&'a [u8]> {
    if input.len() < length {
        return None;
    }
    let (head, tail) = input.split_at(length);
    *input = tail;
    Some(head)
}
fn byte(input: &mut &[u8]) -> Option<usize> {
    Some(take(input, 1)?[0] as usize)
}
fn word(input: &mut &[u8]) -> Option<usize> {
    let b = take(input, 2)?;
    Some(u16::from_be_bytes([b[0], b[1]]) as usize)
}
fn parse_hello(mut input: &[u8]) -> Option<Hello> {
    take(&mut input, 34)?;
    let n = byte(&mut input)?;
    take(&mut input, n)?;
    let n = word(&mut input)?;
    if n == 0 || n % 2 != 0 {
        return None;
    }
    take(&mut input, n)?;
    let n = byte(&mut input)?;
    if n == 0 {
        return None;
    }
    take(&mut input, n)?;
    if input.is_empty() {
        return Some(Hello {
            name: None,
            ech: false,
        });
    }
    let n = word(&mut input)?;
    if n != input.len() {
        return None;
    }
    let mut hello = Hello {
        name: None,
        ech: false,
    };
    let mut seen = std::collections::HashSet::new();
    while !input.is_empty() {
        let kind = word(&mut input)?;
        let n = word(&mut input)?;
        let mut value = take(&mut input, n)?;
        if !seen.insert(kind) {
            return None;
        }
        if kind == 0xfe0d {
            hello.ech = true;
        }
        if kind == 0 {
            let n = word(&mut value)?;
            if n != value.len() {
                return None;
            }
            while !value.is_empty() {
                let kind = byte(&mut value)?;
                let n = word(&mut value)?;
                let name = take(&mut value, n)?;
                if kind == 0 {
                    if hello.name.is_some() {
                        return None;
                    }
                    hello.name = Some(normalize_name(std::str::from_utf8(name).ok()?).ok()?);
                }
            }
        }
    }
    Some(hello)
}
#[cfg(test)]
mod tests {
    use super::*;
    fn record(name: &str, ech: bool) -> Vec<u8> {
        let mut body = vec![3, 3];
        body.extend([0; 32]);
        body.push(0);
        body.extend([0, 2, 0x13, 1, 1, 0]);
        let mut sni = vec![0];
        sni.extend((name.len() as u16).to_be_bytes());
        sni.extend(name.bytes());
        let mut names = (sni.len() as u16).to_be_bytes().to_vec();
        names.extend(sni);
        let mut exts = vec![0, 0];
        exts.extend((names.len() as u16).to_be_bytes());
        exts.extend(names);
        if ech {
            exts.extend([0xfe, 0x0d, 0, 0]);
        }
        body.extend((exts.len() as u16).to_be_bytes());
        body.extend(exts);
        let mut hs = vec![1, 0, (body.len() >> 8) as u8, body.len() as u8];
        hs.extend(body);
        let mut rec = vec![22, 3, 3];
        rec.extend((hs.len() as u16).to_be_bytes());
        rec.extend(hs);
        rec
    }
    #[test]
    fn parses_sni_ech_and_all_partial_prefixes() {
        let r = record("GitHub.COM.", true);
        for n in 0..r.len() {
            assert_eq!(inspect(&r[..n]), Parse::More);
        }
        assert_eq!(
            inspect(&r),
            Parse::Hello(Hello {
                name: Some("github.com".into()),
                ech: true
            })
        );
    }
    #[test]
    fn split_handshake_across_records() {
        let r = record("example.org", false);
        let mut bytes = vec![];
        for chunk in r[5..].chunks(7) {
            bytes.extend([22, 3, 3]);
            bytes.extend((chunk.len() as u16).to_be_bytes());
            bytes.extend(chunk);
        }
        assert!(matches!(inspect(&bytes), Parse::Hello(_)));
    }

    #[test]
    fn incremental_inspector_parses_byte_by_byte() {
        let r = record("example.org", true);
        let mut inspector = Inspector::default();
        for &b in &r[..r.len() - 1] {
            assert_eq!(inspector.feed(&[b]), Parse::More);
        }
        assert_eq!(
            inspector.feed(&r[r.len() - 1..]),
            Parse::Hello(Hello {
                name: Some("example.org".into()),
                ech: true,
            })
        );
    }

    #[test]
    fn rejects_client_hello_exceeding_rfc_maximum() {
        let mut rec = vec![22, 3, 3, 0, 8, 1, 0x0f, 0x00, 0x00, 3, 3, 0, 0];
        assert_eq!(inspect(&rec), Parse::Invalid);
        rec[6] = 0x02;
        rec[7] = 0x02;
        rec[8] = 0x25;
        assert_eq!(inspect(&rec), Parse::Invalid);
    }
}
