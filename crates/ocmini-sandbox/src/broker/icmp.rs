//! Unprivileged host ping sockets. Each echo flow is peer-pinned and expires
//! after the kernel conntrack default of 30 seconds.
use crate::config::{Direction, Protocol};
use futures::{stream::FuturesUnordered, StreamExt};
use std::{
    collections::HashMap,
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    os::fd::FromRawFd,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::net::UdpSocket;
#[derive(Hash, PartialEq, Eq, Clone, Copy)]
struct Key {
    guest: IpAddr,
    remote: IpAddr,
    id: u16,
}
struct Flow {
    socket: Arc<UdpSocket>,
    touched: Instant,
}
#[derive(Default)]
pub struct Echo {
    flows: HashMap<Key, Flow>,
}
impl Echo {
    pub async fn receive(&mut self, packet: &[u8], rules: &Direction) -> io::Result<bool> {
        let Some((guest, remote, offset, protocol)) = header(packet) else {
            return Ok(false);
        };
        if protocol != 1 && protocol != 58 {
            return Ok(false);
        }
        let Some(payload) = packet.get(offset..) else {
            return Ok(true);
        };
        if payload.len() < 8 {
            return Ok(true);
        }
        if payload[0] != (if protocol == 1 { 8 } else { 128 }) {
            return Ok(false);
        }
        let key = Key {
            guest,
            remote,
            id: u16::from_be_bytes([payload[4], payload[5]]),
        };
        if let std::collections::hash_map::Entry::Vacant(e) = self.flows.entry(key) {
            if !rules.permits(Protocol::Icmp, remote, 0, None) {
                return Ok(true);
            }
            let fd = unsafe {
                libc::socket(
                    if remote.is_ipv4() {
                        libc::AF_INET
                    } else {
                        libc::AF_INET6
                    },
                    libc::SOCK_DGRAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                    if remote.is_ipv4() {
                        libc::IPPROTO_ICMP
                    } else {
                        libc::IPPROTO_ICMPV6
                    },
                )
            };
            if fd < 0 {
                return Ok(true);
            }
            let socket = unsafe { std::net::UdpSocket::from_raw_fd(fd) };
            if socket.connect(SocketAddr::new(remote, 0)).is_err() {
                return Ok(true);
            }
            let socket = UdpSocket::from_std(socket)?;
            socket.writable().await?;
            e.insert(Flow {
                socket: Arc::new(socket),
                touched: Instant::now(),
            });
        }
        let flow = self.flows.get_mut(&key).unwrap();
        let _ = flow.socket.try_send(payload);
        flow.touched = Instant::now();
        Ok(true)
    }
    pub fn poll(&mut self) -> Vec<Vec<u8>> {
        if self.flows.is_empty() {
            return Vec::new();
        }
        let mut packets = Vec::new();
        let mut bytes = [0u8; 65535];
        for (key, flow) in &mut self.flows {
            while let Ok(n) = flow.socket.try_recv(&mut bytes) {
                if n < 8 || bytes[0] != (if key.remote.is_ipv4() { 0 } else { 129 }) {
                    continue;
                }
                bytes[4..6].copy_from_slice(&key.id.to_be_bytes());
                bytes[2..4].fill(0);
                let payload = &mut bytes[..n];
                let sum = if let (IpAddr::V6(src), IpAddr::V6(dst)) = (key.remote, key.guest) {
                    let mut pseudo = [0u8; 40];
                    pseudo[..16].copy_from_slice(&src.octets());
                    pseudo[16..32].copy_from_slice(&dst.octets());
                    pseudo[32..36].copy_from_slice(&(n as u32).to_be_bytes());
                    pseudo[36..40].copy_from_slice(&[0, 0, 0, 58]);
                    checksum_parts(&[&pseudo, payload])
                } else {
                    checksum(payload)
                };
                payload[2..4].copy_from_slice(&sum.to_be_bytes());
                packets.extend(ip_packets(
                    key.remote,
                    key.guest,
                    if key.remote.is_ipv4() { 1 } else { 58 },
                    payload,
                ));
                flow.touched = Instant::now();
            }
        }
        self.flows
            .retain(|_, f| f.touched.elapsed() < Duration::from_secs(30));
        packets
    }
    pub fn ready(&self) -> impl std::future::Future<Output = ()> + Send + 'static {
        let futures = FuturesUnordered::new();
        for flow in self.flows.values() {
            let socket = flow.socket.clone();
            futures.push(async move { socket.readable().await });
        }
        async move {
            let mut futures = futures;
            if futures.is_empty() {
                std::future::pending::<()>().await;
            } else {
                let _ = futures.next().await;
            }
        }
    }
}
fn header(p: &[u8]) -> Option<(IpAddr, IpAddr, usize, u8)> {
    match p.first()? >> 4 {
        4 if p.len() >= 20 => {
            let offset = ((p[0] & 15) as usize) * 4;
            if offset < 20 {
                return None;
            }
            Some((
                Ipv4Addr::new(p[12], p[13], p[14], p[15]).into(),
                Ipv4Addr::new(p[16], p[17], p[18], p[19]).into(),
                offset,
                p[9],
            ))
        }
        6 if p.len() >= 40 => Some((
            Ipv6Addr::from(<[u8; 16]>::try_from(&p[8..24]).ok()?).into(),
            Ipv6Addr::from(<[u8; 16]>::try_from(&p[24..40]).ok()?).into(),
            40,
            p[6],
        )),
        _ => None,
    }
}
fn checksum(bytes: &[u8]) -> u16 {
    checksum_parts(&[bytes])
}
fn checksum_parts(parts: &[&[u8]]) -> u16 {
    let mut sum = 0u32;
    for bytes in parts {
        for c in bytes.chunks(2) {
            sum += u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]) as u32;
        }
    }
    while sum >> 16 != 0 {
        sum = (sum & 65535) + (sum >> 16);
    }
    !(sum as u16)
}
fn ip_packets(source: IpAddr, destination: IpAddr, protocol: u8, payload: &[u8]) -> Vec<Vec<u8>> {
    let mut packets = Vec::new();
    match (source, destination) {
        (IpAddr::V4(src), IpAddr::V4(dst)) => {
            static NEXT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);
            let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let size = 1480;
            for (index, chunk) in payload.chunks(size).enumerate() {
                let len = chunk.len() + 20;
                let mut p = vec![0; len];
                p[0] = 0x45;
                p[2..4].copy_from_slice(&(len as u16).to_be_bytes());
                p[4..6].copy_from_slice(&id.to_be_bytes());
                let frag = (index * size / 8) as u16
                    | if (index + 1) * size < payload.len() {
                        0x2000
                    } else {
                        0
                    };
                p[6..8].copy_from_slice(&frag.to_be_bytes());
                p[8] = 64;
                p[9] = protocol;
                p[12..16].copy_from_slice(&src.octets());
                p[16..20].copy_from_slice(&dst.octets());
                let check = checksum(&p[..20]);
                p[10..12].copy_from_slice(&check.to_be_bytes());
                p[20..].copy_from_slice(chunk);
                packets.push(p);
            }
        }
        (IpAddr::V6(src), IpAddr::V6(dst)) if payload.len() + 40 <= 1500 => {
            let mut p = vec![0; 40];
            p[0] = 0x60;
            p[4..6].copy_from_slice(&(payload.len() as u16).to_be_bytes());
            p[6] = protocol;
            p[7] = 64;
            p[8..24].copy_from_slice(&src.octets());
            p[24..40].copy_from_slice(&dst.octets());
            p.extend(payload);
            packets.push(p);
        }
        _ => {}
    }
    packets
}

pub fn echo_reply(packet: &[u8]) -> bool {
    header(packet).is_some_and(|(_, _, offset, proto)| {
        if proto == 1
            && packet.len() >= 8
            && u16::from_be_bytes([packet[6], packet[7]]) & 0x1fff != 0
        {
            return true;
        }
        matches!((proto, packet.get(offset)), (1, Some(0)) | (58, Some(129)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_echo_poll_is_noop_and_split_checksum_matches_contiguous() {
        let mut echo = Echo::default();
        assert!(echo.poll().is_empty());
        let header = [0x20u8; 40];
        for payload in [&b"abcdefgh"[..], &b"odd-length-payload!"[..]] {
            let mut combined = Vec::from(header);
            combined.extend_from_slice(payload);
            assert_eq!(checksum_parts(&[&header, payload]), checksum(&combined));
        }
    }
}
