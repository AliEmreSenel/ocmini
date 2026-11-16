//! Incremental ClientHello inspection, including handshakes split over records.
//! Sizes are TLS wire lengths, never arbitrary parser truncation limits.
use crate::config::normalize_name;
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
pub fn inspect(bytes: &[u8]) -> Parse {
    let mut records = bytes;
    let mut handshake = Vec::new();
    loop {
        if records.len() < 5 {
            return Parse::More;
        }
        if records[0] != 22 || records[1] != 3 {
            return Parse::Invalid;
        }
        let length = u16::from_be_bytes([records[3], records[4]]) as usize;
        if length == 0 || length > 18432 {
            return Parse::Invalid;
        }
        if records.len() < length + 5 {
            return Parse::More;
        }
        handshake.extend_from_slice(&records[5..5 + length]);
        records = &records[5 + length..];
        if handshake.len() >= 4 {
            if handshake[0] != 1 {
                return Parse::Invalid;
            }
            let length = ((handshake[1] as usize) << 16)
                | ((handshake[2] as usize) << 8)
                | handshake[3] as usize;
            if handshake.len() >= 4 + length {
                return parse_hello(&handshake[4..4 + length]).map_or(Parse::Invalid, Parse::Hello);
            }
        }
    }
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
}
