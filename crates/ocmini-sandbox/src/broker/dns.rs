use crate::config::{normalize_name, Direction};
use std::collections::HashMap;
use std::net::{IpAddr, ToSocketAddrs};
use std::time::{Duration, Instant};
type Pins = std::sync::Arc<std::sync::Mutex<HashMap<String, (Instant, Vec<IpAddr>)>>>;
#[derive(Default, Clone)]
pub struct Resolver {
    pins: Pins,
}
impl Resolver {
    pub fn pinned(&self, name: &str, ip: IpAddr) -> bool {
        self.pins
            .lock()
            .unwrap()
            .get(name)
            .is_some_and(|(until, ips)| *until > Instant::now() && ips.contains(&ip))
    }
    pub async fn resolve(&self, name: &str, policy: &Direction) -> Result<Vec<IpAddr>, u8> {
        if !policy.dns_permitted(name) {
            return Err(5);
        }
        {
            let mut pins = self.pins.lock().unwrap();
            pins.retain(|_, (until, _)| *until > Instant::now());
            if let Some((_, ips)) = pins.get(name) {
                return Ok(ips.clone());
            }
        }
        let absolute = format!("{name}.");
        let ips = tokio::task::spawn_blocking(move || {
            (absolute.as_str(), 0)
                .to_socket_addrs()
                .map(|addrs| addrs.map(|a| a.ip()).collect::<Vec<_>>())
        })
        .await
        .map_err(|_| 3u8)?
        .map_err(|_| 3u8)?;
        self.pins.lock().unwrap().insert(
            name.into(),
            (Instant::now() + Duration::from_secs(600), ips.clone()),
        );
        Ok(ips)
    }
    pub async fn answer(&self, query: &[u8], policy: &Direction) -> Vec<u8> {
        let Some((name, kind, end)) = question(query) else {
            return error(query, 1);
        };
        if !policy.dns_permitted(&name) {
            return error(query, 5);
        }
        if !matches!(kind, 1 | 28) {
            return response(query, end, &[], kind);
        }
        match self.resolve(&name, policy).await {
            Ok(ips) => response(query, end, &ips, kind),
            Err(code) => error(query, code),
        }
    }
}
fn question(query: &[u8]) -> Option<(String, u16, usize)> {
    if query.len() < 12 || query[2] & 0xf8 != 0 || query[4..6] != [0, 1] {
        return None;
    }
    let mut at = 12;
    let mut labels = Vec::new();
    loop {
        let n = *query.get(at)? as usize;
        at += 1;
        if n == 0 {
            break;
        }
        if n > 63 {
            return None;
        }
        labels.push(std::str::from_utf8(query.get(at..at + n)?).ok()?);
        at += n;
    }
    let q = query.get(at..at + 4)?;
    if q[2..] != [0, 1] {
        return None;
    }
    Some((
        normalize_name(&labels.join(".")).ok()?,
        u16::from_be_bytes([q[0], q[1]]),
        at + 4,
    ))
}
fn error(query: &[u8], code: u8) -> Vec<u8> {
    let mut out = vec![0; 12];
    if query.len() >= 2 {
        out[..2].copy_from_slice(&query[..2]);
    }
    out[2] = 0x81;
    out[3] = 0x80 | code;
    out
}
fn response(query: &[u8], end: usize, ips: &[IpAddr], kind: u16) -> Vec<u8> {
    let mut out = query[..end].to_vec();
    out[2] = 0x81;
    out[3] = 0x80;
    out[6..12].fill(0);
    let mut count = 0u16;
    for ip in ips {
        let bytes = match (kind, ip) {
            (1, IpAddr::V4(a)) => a.octets().to_vec(),
            (28, IpAddr::V6(a)) => a.octets().to_vec(),
            _ => continue,
        };
        if count == u16::MAX {
            break;
        }
        count += 1;
        out.extend([0xc0, 0x0c]);
        out.extend(kind.to_be_bytes());
        out.extend([0, 1]);
        out.extend(600u32.to_be_bytes());
        out.extend((bytes.len() as u16).to_be_bytes());
        out.extend(bytes);
    }
    out[6..8].copy_from_slice(&count.to_be_bytes());
    out
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Action, Protocol, Rule};
    #[tokio::test]
    async fn denied_names_never_reach_resolver() {
        let policy = Direction::new(Action::Deny);
        let q=b"\x12\x34\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00\x06secret\x07invalid\x00\x00\x01\x00\x01";
        let r = Resolver::default();
        assert_eq!(r.answer(q, &policy).await[3] & 15, 5);
        assert!(r.pins.lock().unwrap().is_empty());
        let mut policy = policy;
        policy.allow.push(Rule {
            protocol: Protocol::Tcp,
            source: None,
            destination: Some("10.0.0.0/8".into()),
            port: None,
        });
        assert!(!policy.dns_permitted("secret.invalid"));
    }
}
