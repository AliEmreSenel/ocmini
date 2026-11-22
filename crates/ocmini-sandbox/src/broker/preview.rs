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
fn ipv6_unavailable(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::EAFNOSUPPORT | libc::EADDRNOTAVAIL)
    )
}

fn guest_destination(bind: SocketAddr) -> SocketAddr {
    let ip = if bind.is_ipv4() {
        IpAddr::V4(super::GUEST4)
    } else {
        IpAddr::V6(super::GUEST6)
    };
    SocketAddr::new(ip, bind.port())
}

fn bind_tcp(addr: SocketAddr) -> io::Result<std::net::TcpListener> {
    match addr {
        SocketAddr::V4(_) => std::net::TcpListener::bind(addr),
        SocketAddr::V6(v6) => {
            use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
            let fd =
                unsafe { libc::socket(libc::AF_INET6, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let socket = unsafe { OwnedFd::from_raw_fd(fd) };
            let one: libc::c_int = 1;
            if unsafe {
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_REUSEADDR,
                    (&one as *const libc::c_int).cast(),
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                )
            } < 0
            {
                return Err(io::Error::last_os_error());
            }
            if unsafe {
                libc::setsockopt(
                    fd,
                    libc::IPPROTO_IPV6,
                    libc::IPV6_V6ONLY,
                    (&one as *const libc::c_int).cast(),
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                )
            } < 0
            {
                return Err(io::Error::last_os_error());
            }
            let mut raw: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
            raw.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            raw.sin6_port = v6.port().to_be();
            raw.sin6_flowinfo = v6.flowinfo();
            raw.sin6_addr.s6_addr = v6.ip().octets();
            raw.sin6_scope_id = v6.scope_id();
            if unsafe {
                libc::bind(
                    fd,
                    (&raw as *const libc::sockaddr_in6).cast(),
                    std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
                )
            } < 0
            {
                return Err(io::Error::last_os_error());
            }
            if unsafe { libc::listen(fd, 128) } < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(unsafe { std::net::TcpListener::from_raw_fd(socket.into_raw_fd()) })
        }
    }
}

fn bind_udp(addr: SocketAddr) -> io::Result<std::net::UdpSocket> {
    match addr {
        SocketAddr::V4(_) => std::net::UdpSocket::bind(addr),
        SocketAddr::V6(v6) => {
            use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
            let fd =
                unsafe { libc::socket(libc::AF_INET6, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let socket = unsafe { OwnedFd::from_raw_fd(fd) };
            let one: libc::c_int = 1;
            if unsafe {
                libc::setsockopt(
                    fd,
                    libc::IPPROTO_IPV6,
                    libc::IPV6_V6ONLY,
                    (&one as *const libc::c_int).cast(),
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                )
            } < 0
            {
                return Err(io::Error::last_os_error());
            }
            let mut raw: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
            raw.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            raw.sin6_port = v6.port().to_be();
            raw.sin6_flowinfo = v6.flowinfo();
            raw.sin6_addr.s6_addr = v6.ip().octets();
            raw.sin6_scope_id = v6.scope_id();
            if unsafe {
                libc::bind(
                    fd,
                    (&raw as *const libc::sockaddr_in6).cast(),
                    std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
                )
            } < 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(unsafe { std::net::UdpSocket::from_raw_fd(socket.into_raw_fd()) })
        }
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
        for binding in policy.network.inbound.inbound_bindings(Protocol::Tcp) {
            let guest = guest_destination(binding.bind);
            if let std::collections::hash_map::Entry::Vacant(entry) = explicit.entry(guest) {
                let listener = match bind_tcp(binding.bind) {
                    Ok(listener) => listener,
                    Err(err) if binding.optional && ipv6_unavailable(&err) => continue,
                    Err(err) => return Err(err),
                };
                listener.set_nonblocking(true)?;
                entry.insert(Arc::new(TcpListener::from_std(listener)?));
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
                        super::GUEST6
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
    listeners: Vec<(SocketAddr, Arc<tokio::net::UdpSocket>)>,
    buf: Vec<u8>,
}
impl UdpIngress {
    pub fn new(policy: &Policy) -> io::Result<Self> {
        let mut listeners = Vec::new();
        let mut seen = HashSet::new();
        for binding in policy.network.inbound.inbound_bindings(Protocol::Udp) {
            let guest = guest_destination(binding.bind);
            if seen.insert(guest) {
                let socket = match bind_udp(binding.bind) {
                    Ok(socket) => socket,
                    Err(err) if binding.optional && ipv6_unavailable(&err) => continue,
                    Err(err) => return Err(err),
                };
                socket.set_nonblocking(true)?;
                listeners.push((guest, Arc::new(tokio::net::UdpSocket::from_std(socket)?)));
            }
        }
        let buf = if listeners.is_empty() {
            Vec::new()
        } else {
            vec![0; 65535]
        };
        Ok(Self { listeners, buf })
    }
    pub async fn receive(
        &mut self,
    ) -> io::Result<(Arc<tokio::net::UdpSocket>, SocketAddr, SocketAddr, &[u8])> {
        if self.listeners.is_empty() {
            return std::future::pending().await;
        }
        loop {
            let mut ready = FuturesUnordered::new();
            for (index, (_, socket)) in self.listeners.iter().enumerate() {
                let socket = socket.clone();
                ready.push(async move {
                    socket.readable().await?;
                    Ok::<_, io::Error>(index)
                });
            }
            let index = ready.next().await.unwrap()?;
            let (destination, socket) = &self.listeners[index];
            match socket.try_recv_from(&mut self.buf) {
                Ok((n, peer)) => {
                    return Ok((socket.clone(), peer, *destination, &self.buf[..n]));
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(e) => return Err(e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Action, Buffers, Direction, Env, Fs, NetMode, Network, Preview, Rule};

    fn test_policy(inbound: Direction) -> Policy {
        Policy {
            name: "test".into(),
            fs: Fs::default(),
            tmp: false,
            process_isolation: crate::config::ProcessIsolation::None,
            unix_sockets: crate::config::UnixSockets::Off,
            io_uring: true,
            env: Env {
                clear: true,
                allow: vec![],
                deny: vec![],
            },
            network: Network {
                mode: NetMode::Firewall,
                ech: crate::config::Ech::Allow,
                preview: Preview {
                    mode: PreviewMode::Off,
                },
                inbound,
                outbound: Direction::new(Action::Deny),
            },
            buffers: Buffers::default(),
        }
    }

    fn free_tcp_port(ip: IpAddr) -> u16 {
        let listener = bind_tcp(SocketAddr::new(ip, 0)).unwrap();
        listener.local_addr().unwrap().port()
    }

    fn free_udp_port(ip: IpAddr) -> u16 {
        let socket = bind_udp(SocketAddr::new(ip, 0)).unwrap();
        socket.local_addr().unwrap().port()
    }

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

    #[tokio::test]
    async fn loopback_inbound_rules_bind_loopback_not_wildcard_for_ipv4_and_ipv6() {
        let port4 = free_tcp_port(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let mut inbound4 = Direction::new(Action::Deny);
        inbound4.allow.push(Rule {
            protocol: Protocol::Tcp,
            source: Some("127.0.0.1".into()),
            destination: None,
            port: Some(port4),
            ..Default::default()
        });
        let pub4 = Publisher::new(0, &test_policy(inbound4)).unwrap();
        assert_eq!(pub4.explicit.len(), 1);
        let guest4 = SocketAddr::new(super::super::GUEST4.into(), port4);
        assert_eq!(
            pub4.explicit[&guest4].local_addr().unwrap(),
            SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port4)
        );

        let port6 = free_tcp_port(IpAddr::V6(Ipv6Addr::LOCALHOST));
        let mut inbound6 = Direction::new(Action::Deny);
        inbound6.allow.push(Rule {
            protocol: Protocol::Tcp,
            source: Some("::1".into()),
            destination: None,
            port: Some(port6),
            ..Default::default()
        });
        let pub6 = Publisher::new(0, &test_policy(inbound6)).unwrap();
        assert_eq!(pub6.explicit.len(), 1);
        let guest6 = SocketAddr::new(super::super::GUEST6.into(), port6);
        assert_eq!(
            pub6.explicit[&guest6].local_addr().unwrap(),
            SocketAddr::new(Ipv6Addr::LOCALHOST.into(), port6)
        );

        let udp_port6 = free_udp_port(IpAddr::V6(Ipv6Addr::LOCALHOST));
        let mut udp_inbound6 = Direction::new(Action::Deny);
        udp_inbound6.allow.push(Rule {
            protocol: Protocol::Udp,
            source: Some("::1".into()),
            destination: None,
            port: Some(udp_port6),
            ..Default::default()
        });
        let udp6 = UdpIngress::new(&test_policy(udp_inbound6)).unwrap();
        assert_eq!(udp6.listeners.len(), 1);
        assert_eq!(
            udp6.listeners[0].0,
            SocketAddr::new(super::super::GUEST6.into(), udp_port6)
        );
        assert_eq!(
            udp6.listeners[0].1.local_addr().unwrap(),
            SocketAddr::new(Ipv6Addr::LOCALHOST.into(), udp_port6)
        );
    }

    #[tokio::test]
    async fn wildcard_inbound_binds_both_ipv4_and_ipv6_v6only_without_conflict() {
        let port = free_tcp_port(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        let mut inbound = Direction::new(Action::Deny);
        inbound.allow.push(Rule {
            protocol: Protocol::Tcp,
            source: None,
            destination: None,
            port: Some(port),
            ..Default::default()
        });
        let publisher = Publisher::new(0, &test_policy(inbound)).unwrap();
        let guest4 = SocketAddr::new(super::super::GUEST4.into(), port);
        let guest6 = SocketAddr::new(super::super::GUEST6.into(), port);
        assert_eq!(
            publisher.explicit[&guest4].local_addr().unwrap(),
            SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), port)
        );
        if let Some(listener6) = publisher.explicit.get(&guest6) {
            assert_eq!(
                listener6.local_addr().unwrap(),
                SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), port)
            );
        }
    }
}
