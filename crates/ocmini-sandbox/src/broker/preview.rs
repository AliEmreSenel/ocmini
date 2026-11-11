use crate::config::{Policy, PreviewMode, Protocol};
use futures::{stream::FuturesUnordered, StreamExt};
use std::{
    collections::{HashMap, HashSet},
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
};
use tokio::net::{TcpListener, TcpStream};
pub const LEASE_PORT: u16 = 32765;
struct Lease {
    address: Ipv4Addr,
    _marker: std::net::TcpListener,
}
impl Lease {
    fn acquire() -> io::Result<Self> {
        for index in 2u32..65535 {
            let address = Ipv4Addr::new(127, 80, (index >> 8) as u8, index as u8);
            match std::net::TcpListener::bind((address, LEASE_PORT)) {
                Ok(marker) => {
                    return Ok(Self {
                        address,
                        _marker: marker,
                    })
                }
                Err(e) if e.kind() == io::ErrorKind::AddrInUse => continue,
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            "preview lease range exhausted",
        ))
    }
}
pub struct Publisher {
    pid: i32,
    lease: Option<Lease>,
    listeners: HashMap<SocketAddr, Arc<TcpListener>>,
    explicit: HashMap<SocketAddr, Arc<TcpListener>>,
    enabled: bool,
}
impl Publisher {
    pub fn new(pid: i32, policy: &Policy) -> io::Result<Self> {
        let enabled = policy.network.preview.mode == PreviewMode::AutoLoopback;
        let lease = if enabled {
            Some(Lease::acquire()?)
        } else {
            None
        };
        let mut explicit = HashMap::new();
        for rule in &policy.network.inbound.allow {
            if matches!(rule.protocol, Protocol::Tcp | Protocol::Any) {
                if let Some(port) = rule.port {
                    let guest = SocketAddr::new(super::GUEST4.into(), port);
                    if let std::collections::hash_map::Entry::Vacant(entry) = explicit.entry(guest)
                    {
                        let listener = std::net::TcpListener::bind((Ipv4Addr::UNSPECIFIED, port))?;
                        listener.set_nonblocking(true)?;
                        entry.insert(Arc::new(TcpListener::from_std(listener)?));
                    }
                }
            }
        }
        Ok(Self {
            pid,
            lease,
            listeners: HashMap::new(),
            explicit,
            enabled,
        })
    }
    pub fn discover(&mut self, policy: &Policy) -> io::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let Some(lease) = &self.lease else {
            return Ok(());
        };
        let mut live = HashSet::new();
        for (file, ipv6) in [("tcp", false), ("tcp6", true)] {
            let table = std::fs::read_to_string(format!("/proc/{}/net/{file}", self.pid))?;
            for line in table.lines().skip(1) {
                let cols: Vec<_> = line.split_whitespace().collect();
                if cols.len() < 4 || cols[3] != "0A" {
                    continue;
                }
                let Some((hex, port)) = cols[1].split_once(':') else {
                    continue;
                };
                let Ok(port) = u16::from_str_radix(port, 16) else {
                    continue;
                };
                if !policy.network.inbound.permits(
                    Protocol::Tcp,
                    IpAddr::V4(Ipv4Addr::LOCALHOST),
                    port,
                    None,
                ) {
                    continue;
                }
                let address = if ipv6 {
                    let Some(ip) = ipv6_address(hex) else {
                        continue;
                    };
                    if ip == Ipv6Addr::LOCALHOST {
                        continue;
                    }
                    IpAddr::V6(if ip.is_unspecified() {
                        Ipv6Addr::new(0xfd00, 0x6f63, 0x6d69, 0, 0, 0, 0, 1)
                    } else {
                        ip
                    })
                } else {
                    let Ok(ip) = u32::from_str_radix(hex, 16) else {
                        continue;
                    };
                    let ip = Ipv4Addr::from(ip.to_ne_bytes());
                    IpAddr::V4(if ip.is_unspecified() {
                        super::GUEST4
                    } else {
                        ip
                    })
                };
                let destination = SocketAddr::new(address, port);
                live.insert(destination);
                if self.listeners.contains_key(&destination) {
                    continue;
                }
                let listener = match std::net::TcpListener::bind((lease.address, port)) {
                    Ok(s) => s,
                    Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
                        std::net::TcpListener::bind((lease.address, 0))?
                    }
                    Err(e) => return Err(e),
                };
                let actual = listener.local_addr()?;
                listener.set_nonblocking(true)?;
                self.listeners
                    .insert(destination, Arc::new(TcpListener::from_std(listener)?));
                eprintln!("ocmini: preview http://{actual} -> {destination}");
            }
        }
        self.listeners
            .retain(|destination, _| live.contains(destination));
        Ok(())
    }
    pub fn accept(
        &self,
    ) -> impl std::future::Future<
        Output = io::Result<Option<(TcpStream, SocketAddr, SocketAddr, bool)>>,
    > + Send
           + 'static {
        let futures = FuturesUnordered::new();
        for (destination, listener, preview) in self
            .listeners
            .iter()
            .map(|(d, l)| (d, l, true))
            .chain(self.explicit.iter().map(|(d, l)| (d, l, false)))
        {
            let listener = listener.clone();
            let destination = *destination;
            futures.push(async move {
                listener
                    .accept()
                    .await
                    .map(|(stream, peer)| Some((stream, peer, destination, preview)))
            });
        }
        async move {
            let mut futures = futures;
            if futures.is_empty() {
                std::future::pending().await
            } else {
                futures.next().await.unwrap_or(Ok(None))
            }
        }
    }
}
fn ipv6_address(hex: &str) -> Option<Ipv6Addr> {
    if hex.len() != 32 {
        return None;
    }
    let mut bytes = [0; 16];
    for i in 0..4 {
        bytes[i * 4..i * 4 + 4].copy_from_slice(
            &u32::from_str_radix(&hex[i * 8..i * 8 + 8], 16)
                .ok()?
                .to_ne_bytes(),
        );
    }
    Some(Ipv6Addr::from(bytes))
}
pub struct UdpIngress {
    listeners: Vec<(u16, Arc<tokio::net::UdpSocket>)>,
}
impl UdpIngress {
    pub fn new(policy: &Policy) -> io::Result<Self> {
        let mut listeners = Vec::new();
        let mut ports = HashSet::new();
        for rule in &policy.network.inbound.allow {
            if matches!(rule.protocol, Protocol::Udp | Protocol::Any) {
                if let Some(port) = rule.port {
                    if ports.insert(port) {
                        let socket = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port))?;
                        socket.set_nonblocking(true)?;
                        listeners.push((port, Arc::new(tokio::net::UdpSocket::from_std(socket)?)));
                    }
                }
            }
        }
        Ok(Self { listeners })
    }
    pub fn receive(
        &self,
    ) -> impl std::future::Future<
        Output = io::Result<(Arc<tokio::net::UdpSocket>, SocketAddr, SocketAddr, Vec<u8>)>,
    > + Send
           + 'static {
        let futures = FuturesUnordered::new();
        for (port, socket) in &self.listeners {
            let socket = socket.clone();
            let port = *port;
            futures.push(async move {
                loop {
                    socket.readable().await?;
                    let mut bytes = vec![0; 65535];
                    match socket.try_recv_from(&mut bytes) {
                        Ok((n, peer)) => {
                            bytes.truncate(n);
                            return Ok((
                                socket,
                                peer,
                                SocketAddr::new(super::GUEST4.into(), port),
                                bytes,
                            ));
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                        Err(e) => return Err(e),
                    }
                }
            });
        }
        async move {
            let mut futures = futures;
            if futures.is_empty() {
                std::future::pending().await
            } else {
                futures.next().await.unwrap()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn leases_are_exclusive_and_reusable() {
        let a = Lease::acquire().unwrap();
        let addr = a.address;
        let b = Lease::acquire().unwrap();
        assert_ne!(addr, b.address);
        drop(a);
        let c = Lease::acquire().unwrap();
        assert_eq!(addr, c.address);
    }
}
