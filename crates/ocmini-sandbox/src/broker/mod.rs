//! Transparent TUN relay. All real destination sockets are owned by the brain.
mod buffers;
mod dns;
mod icmp;
mod preview;
mod tls;
use crate::{
    config::{Ech, Policy, Protocol},
    wire,
};
use futures::{stream::FuturesUnordered, StreamExt};
use smoltcp::{
    iface::{Config, Interface, SocketHandle, SocketSet},
    phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken},
    socket::{raw, tcp, udp},
    time::Instant as StackTime,
    wire::{HardwareAddress, IpAddress, IpCidr, IpEndpoint, IpProtocol, IpVersion},
};
use std::{
    collections::{HashMap, VecDeque},
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    time::{Duration, Instant},
};
use tokio::net::{TcpStream, UdpSocket};
use tokio_util::sync::CancellationToken;
const PACKET_POOL: usize = 64;
pub const GUEST4: Ipv4Addr = Ipv4Addr::new(198, 18, 0, 1);
pub const PEER4: Ipv4Addr = Ipv4Addr::new(198, 18, 0, 2);
const PEER6: std::net::Ipv6Addr = std::net::Ipv6Addr::new(0xfd00, 0x6f63, 0x6d69, 0, 0, 0, 0, 2);
#[derive(Default)]
struct PacketDevice {
    incoming: Option<Vec<u8>>,
    outgoing: VecDeque<Vec<u8>>,
}
struct Receive(Vec<u8>);
struct Transmit<'a>(&'a mut VecDeque<Vec<u8>>);
impl RxToken for Receive {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.0)
    }
}
impl TxToken for Transmit<'_> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut bytes = vec![0; len];
        let result = f(&mut bytes);
        self.0.push_back(bytes);
        result
    }
}
impl Device for PacketDevice {
    type RxToken<'a> = Receive;
    type TxToken<'a> = Transmit<'a>;
    fn receive(&mut self, _: StackTime) -> Option<(Receive, Transmit<'_>)> {
        if self.outgoing.len() >= PACKET_POOL {
            return None;
        }
        self.incoming
            .take()
            .map(|p| (Receive(p), Transmit(&mut self.outgoing)))
    }
    fn transmit(&mut self, _: StackTime) -> Option<Transmit<'_>> {
        if self.outgoing.len() < PACKET_POOL {
            Some(Transmit(&mut self.outgoing))
        } else {
            None
        }
    }
    fn capabilities(&self) -> DeviceCapabilities {
        let mut c = DeviceCapabilities::default();
        c.medium = Medium::Ip;
        c.max_transmission_unit = wire::MTU;
        c.max_burst_size = Some(PACKET_POOL);
        c
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Tuple {
    guest: SocketAddr,
    remote: SocketAddr,
}
struct TcpFlow {
    handle: SocketHandle,
    host: Option<std::sync::Arc<TcpStream>>,
    connecting: bool,
    inspect: bool,
    prefix: Vec<u8>,
    sent: usize,
    read_eof: bool,
    write_eof: bool,
    touched: Instant,
    _buffers: buffers::TcpBuffers,
}
struct UdpFlow {
    reply_to: Option<SocketAddr>,
    host: std::sync::Arc<UdpSocket>,
    handle: SocketHandle,
    touched: Instant,
    answered: bool,
}
struct UdpListener {
    touched: Instant,
    handle: SocketHandle,
    address: SocketAddr,
}
fn endpoint(addr: SocketAddr) -> IpEndpoint {
    IpEndpoint::new(IpAddress::from(addr.ip()), addr.port())
}
fn address(ep: IpEndpoint) -> SocketAddr {
    SocketAddr::new(ep.addr.into(), ep.port)
}
fn socket(policy: &Policy) -> io::Result<(tcp::Socket<'static>, buffers::TcpBuffers)> {
    buffers::TcpBuffers::socket(policy.buffers.tcp_recv, policy.buffers.tcp_send)
}
fn stamp(start: Instant) -> StackTime {
    StackTime::from_millis(start.elapsed().as_millis() as i64)
}

pub async fn run(
    packets: std::os::unix::net::UnixStream,
    host_pid: i32,
    policy: Policy,
    cancel: CancellationToken,
) -> io::Result<()> {
    packets.set_nonblocking(true)?;
    let packets = tokio::net::UnixStream::from_std(packets)?;
    let (reader, mut writer) = packets.into_split();
    let mut frames = wire::Frames::new(reader, b'P');
    let start = Instant::now();
    let mut device = PacketDevice::default();
    let mut config = Config::new(HardwareAddress::Ip);
    config.random_seed = host_pid as u64 ^ start.elapsed().as_nanos() as u64;
    let mut iface = Interface::new(config, &mut device, stamp(start));
    iface.set_any_ip(true);
    iface.update_ip_addrs(|addrs| {
        addrs.push(IpCidr::new(PEER4.into(), 15)).unwrap();
        addrs.push(IpCidr::new(PEER6.into(), 64)).unwrap();
    });
    iface
        .routes_mut()
        .add_default_ipv4_route(PEER4)
        .map_err(io::Error::other)?;
    iface
        .routes_mut()
        .add_default_ipv6_route(PEER6)
        .map_err(io::Error::other)?;
    // Drop sockets before their demand-paged backing allocations.
    let mut tcp_flows: HashMap<Tuple, TcpFlow> = HashMap::new();
    // Latest guest SYN per flow whose host connect is still in flight. The
    // guest handshake completes only after the real destination accepts, so
    // unreachable addresses are refused and clients can try another family.
    let mut pending: HashMap<Tuple, Vec<u8>> = HashMap::new();
    let mut udp_flows: HashMap<Tuple, UdpFlow> = HashMap::new();
    let mut udp_listeners: HashMap<SocketAddr, UdpListener> = HashMap::new();
    let mut sockets = SocketSet::new(vec![]);
    let mut connects: FuturesUnordered<
        futures::future::BoxFuture<'static, (Tuple, io::Result<TcpStream>)>,
    > = FuturesUnordered::new();
    let mut dns_jobs = FuturesUnordered::new();
    let resolver = dns::Resolver::default();
    let mut previews = preview::Publisher::new(host_pid, &policy)?;
    let mut udp_ingress = preview::UdpIngress::new(&policy)?;
    let mut discovery = tokio::time::interval(Duration::from_millis(250));
    let mut udp_bytes = vec![0; 65535];
    let mut echoes = icmp::Echo::default();
    let mut icmp4 = None;
    let mut icmp6 = None;
    loop {
        if cancel.is_cancelled() {
            break;
        }
        iface.poll(stamp(start), &mut device, &mut sockets);
        for handle in [icmp4, icmp6].into_iter().flatten() {
            let sock = sockets.get_mut::<raw::Socket>(handle);
            while sock.can_recv() {
                let packet = sock.recv().map_err(io::Error::other)?;
                echoes.receive(packet, &policy.network.outbound).await?;
            }
        }
        // The stack's local echo responder must not impersonate external hosts.
        // Only replies returned by the real host ping socket are forwarded.
        device.outgoing.retain(|packet| !icmp::echo_reply(packet));
        let mut progress = false;
        for (key, flow) in &mut tcp_flows {
            let sock = sockets.get_mut::<tcp::Socket>(flow.handle);
            if flow.inspect && sock.can_recv() {
                let mut bytes = vec![0; sock.recv_queue()];
                let n = sock.recv_slice(&mut bytes).unwrap_or(0);
                bytes.truncate(n);
                flow.prefix.extend(bytes);
                flow.touched = Instant::now();
                progress = n > 0;
                match tls::inspect(&flow.prefix) {
                    tls::Parse::More => {}
                    tls::Parse::Invalid => {
                        sock.abort();
                        flow.inspect = false;
                    }
                    tls::Parse::Hello(hello) => {
                        let name = hello.name.as_deref();
                        let outbound = &policy.network.outbound;
                        let denied = outbound.deny.iter().any(|r| {
                            r.matches(Protocol::Tcp, key.remote.ip(), key.remote.port(), name)
                        });
                        flow.inspect = false;
                        if denied || (hello.ech && policy.network.ech == Ech::Deny) {
                            sock.abort();
                        } else {
                            flow.connecting = true;
                            let key = *key;
                            let rules = outbound.clone();
                            let resolver = resolver.clone();
                            connects.push(Box::pin(async move {
                                let result = async {
                                    if !rules.permits(
                                        Protocol::Tcp,
                                        key.remote.ip(),
                                        key.remote.port(),
                                        None,
                                    ) {
                                        let name = hello.name.as_deref().ok_or_else(|| {
                                            io::Error::from(io::ErrorKind::PermissionDenied)
                                        })?;
                                        if !resolver.pinned(name, key.remote.ip()) {
                                            let _ = resolver.resolve(name, &rules).await;
                                        }
                                        if !resolver.pinned(name, key.remote.ip())
                                            || !rules.permits(
                                                Protocol::Tcp,
                                                key.remote.ip(),
                                                key.remote.port(),
                                                Some(name),
                                            )
                                        {
                                            return Err(io::Error::from(
                                                io::ErrorKind::PermissionDenied,
                                            ));
                                        }
                                    }
                                    TcpStream::connect(key.remote).await
                                }
                                .await;
                                (key, result)
                            }));
                        }
                    }
                }
            }
            if let Some(host) = &flow.host {
                if flow.sent < flow.prefix.len() {
                    match host.try_write(&flow.prefix[flow.sent..]) {
                        Ok(0) => {
                            sock.abort();
                        }
                        Ok(n) => {
                            flow.sent += n;
                            flow.touched = Instant::now();
                            progress = true;
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                        Err(_) => sock.abort(),
                    }
                } else if sock.can_recv() {
                    let wrote = sock
                        .recv(|bytes| match host.try_write(bytes) {
                            Ok(n) => (n, n),
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => (0, 0),
                            Err(_) => (0, usize::MAX),
                        })
                        .unwrap_or(0);
                    if wrote == usize::MAX {
                        sock.abort();
                    } else if wrote > 0 {
                        progress = true;
                        flow.touched = Instant::now();
                    }
                }
                if sock.can_send() && !flow.read_eof {
                    let read = sock
                        .send(|bytes| match host.try_read(bytes) {
                            Ok(0) => (0, Some(0)),
                            Ok(n) => (n, Some(n)),
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => (0, None),
                            Err(_) => (0, Some(0)),
                        })
                        .ok()
                        .flatten();
                    match read {
                        Some(0) => {
                            flow.read_eof = true;
                            sock.close();
                            progress = true;
                        }
                        Some(_) => {
                            flow.touched = Instant::now();
                            progress = true;
                        }
                        None => {}
                    }
                }
                if !sock.may_recv()
                    && sock.recv_queue() == 0
                    && !flow.write_eof
                    && !matches!(sock.state(), tcp::State::SynSent | tcp::State::SynReceived)
                {
                    unsafe {
                        use std::os::fd::AsRawFd;
                        libc::shutdown(host.as_raw_fd(), libc::SHUT_WR);
                    }
                    flow.write_eof = true;
                }
            }
            let timeout = match sock.state() {
                tcp::State::SynSent => 120,
                tcp::State::SynReceived => 60,
                tcp::State::Established => 432000,
                tcp::State::FinWait1 | tcp::State::FinWait2 | tcp::State::TimeWait => 120,
                tcp::State::CloseWait => 60,
                tcp::State::LastAck => 30,
                tcp::State::Closed => 10,
                _ => 300,
            };
            if flow.touched.elapsed() > Duration::from_secs(timeout) {
                sock.abort();
            }
        }
        let closed: Vec<_> = tcp_flows
            .iter()
            .filter(|(_, f)| !sockets.get::<tcp::Socket>(f.handle).is_open())
            .map(|(k, _)| *k)
            .collect();
        for key in closed {
            let f = tcp_flows.remove(&key).unwrap();
            sockets.remove(f.handle);
            drop(f);
        }
        for listener in udp_listeners.values() {
            let sock = sockets.get_mut::<udp::Socket>(listener.handle);
            while sock.can_recv() {
                let (payload, metadata) = sock.recv().map_err(io::Error::other)?;
                let guest = address(metadata.endpoint);
                let key = Tuple {
                    guest,
                    remote: listener.address,
                };
                if listener.address.port() == 53
                    && (listener.address.ip() == IpAddr::V4(PEER4)
                        || listener.address.ip() == IpAddr::V6(PEER6))
                {
                    let resolver = resolver.clone();
                    let query = payload.to_vec();
                    let rules = policy.network.outbound.clone();
                    let handle = listener.handle;
                    dns_jobs.push(
                        async move { (handle, guest, resolver.answer(&query, &rules).await) },
                    );
                    continue;
                }
                if let std::collections::hash_map::Entry::Vacant(e) = udp_flows.entry(key) {
                    if !policy.network.outbound.permits(
                        Protocol::Udp,
                        key.remote.ip(),
                        key.remote.port(),
                        None,
                    ) {
                        continue;
                    }
                    let host = std::net::UdpSocket::bind(if key.remote.is_ipv4() {
                        "0.0.0.0:0"
                    } else {
                        "[::]:0"
                    })?;
                    if host.connect(key.remote).is_err() {
                        continue;
                    }
                    host.set_nonblocking(true)?;
                    let host = UdpSocket::from_std(host)?;
                    host.writable().await?;
                    e.insert(UdpFlow {
                        host: std::sync::Arc::new(host),
                        reply_to: None,
                        handle: listener.handle,
                        touched: Instant::now(),
                        answered: false,
                    });
                }
                let flow = udp_flows.get_mut(&key).unwrap();
                let _ = match flow.reply_to {
                    Some(peer) => flow.host.try_send_to(payload, peer),
                    None => flow.host.try_send(payload),
                };
                flow.touched = Instant::now();
                progress = true;
            }
        }
        for (key, flow) in &mut udp_flows {
            if flow.reply_to.is_some() {
                continue;
            }
            loop {
                match flow.host.try_recv(&mut udp_bytes) {
                    Ok(n) => {
                        let sock = sockets.get_mut::<udp::Socket>(flow.handle);
                        let _ = sock.send_slice(&udp_bytes[..n], endpoint(key.guest));
                        flow.touched = Instant::now();
                        flow.answered = true;
                        progress = true;
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }
        }
        udp_flows.retain(|_, flow| {
            flow.touched.elapsed() < Duration::from_secs(if flow.answered { 120 } else { 30 })
        });
        let stale: Vec<_> = udp_listeners
            .iter()
            .filter(|(address, listener)| {
                address.port() != 53
                    && listener.touched.elapsed() > Duration::from_secs(30)
                    && !udp_flows
                        .values()
                        .any(|flow| flow.handle == listener.handle)
                    && !sockets.get::<udp::Socket>(listener.handle).can_recv()
            })
            .map(|(address, _)| *address)
            .collect();
        for address in stale {
            if let Some(listener) = udp_listeners.remove(&address) {
                sockets.remove(listener.handle);
            }
        }
        for packet in echoes.poll() {
            device.outgoing.push_back(packet);
        }
        while let Some(packet) = device.outgoing.pop_front() {
            wire::write_frame(&mut writer, b'P', &packet).await?;
        }
        if progress {
            continue;
        }
        let mut ready = FuturesUnordered::new();
        for flow in tcp_flows.values() {
            if let Some(host) = &flow.host {
                let sock = sockets.get::<tcp::Socket>(flow.handle);
                let mut interest = tokio::io::Interest::ERROR;
                if sock.can_send() && !flow.read_eof {
                    interest |= tokio::io::Interest::READABLE;
                }
                if sock.can_recv() || flow.sent < flow.prefix.len() {
                    interest |= tokio::io::Interest::WRITABLE;
                }
                let host = host.clone();
                ready.push(async move { host.ready(interest).await });
            }
        }
        let mut udp_ready = FuturesUnordered::new();
        for flow in udp_flows.values().filter(|f| f.reply_to.is_none()) {
            let host = flow.host.clone();
            udp_ready.push(async move { host.readable().await });
        }
        let delay = iface
            .poll_delay(stamp(start), &sockets)
            .map(|d| Duration::from_micros(d.total_micros()))
            .unwrap_or(Duration::from_secs(432000));
        tokio::select! {
            _=cancel.cancelled()=>break,
            packet=frames.frame()=>{
                let packet=packet?;
                if let Some((version,proto,slot))=match packet.first().map(|b|b>>4){Some(4) if packet.get(9)==Some(&1)=>Some((IpVersion::Ipv4,IpProtocol::Icmp,&mut icmp4)),Some(6) if packet.get(6)==Some(&58)=>Some((IpVersion::Ipv6,IpProtocol::Icmpv6,&mut icmp6)),_=>None}{
                    if slot.is_none(){let ring=policy.buffers.udp_ring;let bytes=ring.checked_mul(wire::MTU).ok_or_else(||io::Error::other("packet buffer overflow"))?.max(65535);let sock=raw::Socket::new(version,proto,raw::PacketBuffer::new(vec![raw::PacketMetadata::EMPTY;ring],vec![0;bytes]),raw::PacketBuffer::new(vec![],vec![]));*slot=Some(sockets.add(sock));}
                }
                if let Some((protocol,key,syn))=tuple(&packet){
                    if protocol==Protocol::Tcp{if let Some(held)=pending.get_mut(&key){if syn{*held=packet;}continue;}}
                    if let Some(flow)=tcp_flows.get_mut(&key){flow.touched=Instant::now();}
                    if protocol==Protocol::Tcp && syn && !tcp_flows.contains_key(&key){
                        let inspect=key.remote.port()==443 && (policy.network.outbound.inspects_tls() || policy.network.ech==Ech::Deny);
                        if inspect{
                            let(mut sock,backing)=socket(&policy)?;sock.listen(endpoint(key.remote)).map_err(io::Error::other)?;let handle=sockets.add(sock);
                            tcp_flows.insert(key,TcpFlow{handle,host:None,connecting:false,inspect,prefix:vec![],sent:0,read_eof:false,write_eof:false,touched:Instant::now(),_buffers:backing});
                        }else if policy.network.outbound.permits(protocol,key.remote.ip(),key.remote.port(),None){
                            pending.insert(key,packet);
                            connects.push(Box::pin(async move{(key,TcpStream::connect(key.remote).await)}));
                            continue;
                        }
                    }else if protocol==Protocol::Udp && !udp_listeners.contains_key(&key.remote){
                        let ring=policy.buffers.udp_ring;let bytes=ring.checked_mul(wire::MTU).ok_or_else(||io::Error::other("UDP buffer overflow"))?.max(65535);
                        let mut sock=udp::Socket::new(udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY;ring],vec![0;bytes]),udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY;ring],vec![0;bytes]));sock.bind(endpoint(key.remote)).map_err(io::Error::other)?;
                        let handle=sockets.add(sock);udp_listeners.insert(key.remote,UdpListener{touched:Instant::now(),handle,address:key.remote});
                    }
                }
                device.incoming=Some(packet);
            },
            Some((key,result))=connects.next(),if !connects.is_empty()=>{if let Some(syn)=pending.remove(&key){
                if let Ok(host)=result{
                    host.set_nodelay(true)?;
                    let(mut sock,backing)=socket(&policy)?;sock.listen(endpoint(key.remote)).map_err(io::Error::other)?;let handle=sockets.add(sock);
                    tcp_flows.insert(key,TcpFlow{handle,host:Some(std::sync::Arc::new(host)),connecting:false,inspect:false,prefix:vec![],sent:0,read_eof:false,write_eof:false,touched:Instant::now(),_buffers:backing});
                }
                // Replay the held SYN: the new listener answers it, or with no
                // listener the stack resets it. A packet already queued wins;
                // the guest retransmits its SYN.
                if device.incoming.is_none(){device.incoming=Some(syn);}
            }else if let Some(flow)=tcp_flows.get_mut(&key){flow.connecting=false;match result{Ok(host)=>{host.set_nodelay(true)?;flow.host=Some(std::sync::Arc::new(host));},Err(_)=>sockets.get_mut::<tcp::Socket>(flow.handle).abort()}}},
            Some((handle,guest,answer))=dns_jobs.next(),if !dns_jobs.is_empty()=>{let _=sockets.get_mut::<udp::Socket>(handle).send_slice(&answer,endpoint(guest));},
            _=ready.next(),if !ready.is_empty()=>{},
            _=udp_ready.next(),if !udp_ready.is_empty()=>{},
            _=echoes.ready()=>{},
            _=discovery.tick()=>{previews.discover(&policy)?;},
            datagram=udp_ingress.receive()=>{
                let(host,peer,destination,bytes)=datagram?;
                if !policy.network.inbound.permits(Protocol::Udp,peer.ip(),destination.port(),None){continue;}
                let source=if peer.ip().is_loopback(){SocketAddr::new(PEER4.into(),peer.port())}else{peer};
                let key=Tuple{guest:destination,remote:source};
                if let std::collections::hash_map::Entry::Vacant(e) = udp_listeners.entry(source) {let ring=policy.buffers.udp_ring;let capacity=ring.checked_mul(wire::MTU).ok_or_else(||io::Error::other("UDP buffer overflow"))?.max(65535);let mut sock=udp::Socket::new(udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY;ring],vec![0;capacity]),udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY;ring],vec![0;capacity]));sock.bind(endpoint(source)).map_err(io::Error::other)?;let handle=sockets.add(sock);e.insert(UdpListener{touched:Instant::now(),handle,address:source});}
                let handle=udp_listeners[&source].handle;let _=sockets.get_mut::<udp::Socket>(handle).send_slice(bytes,endpoint(destination));
                udp_flows.insert(key,UdpFlow{host,handle,reply_to:Some(peer),touched:Instant::now(),answered:false});
            },
            accepted=previews.accept()=>{
                if let Some((host,peer,destination,preview))=accepted?{
                    if !policy.network.inbound.permits(Protocol::Tcp,peer.ip(),destination.port(),None){continue;}
                    let source=if !preview && !peer.ip().is_loopback(){peer}else if destination.is_ipv4(){SocketAddr::new(PEER4.into(),peer.port())}else{SocketAddr::new(PEER6.into(),peer.port())};
                    let key=Tuple{guest:destination,remote:source};if tcp_flows.contains_key(&key){continue;}
                    let(mut sock,backing)=socket(&policy)?;sock.connect(iface.context(),endpoint(destination),endpoint(source)).map_err(io::Error::other)?;let handle=sockets.add(sock);
                    tcp_flows.insert(key,TcpFlow{handle,host:Some(std::sync::Arc::new(host)),connecting:false,inspect:false,prefix:vec![],sent:0,read_eof:false,write_eof:false,touched:Instant::now(),_buffers:backing});
                }
            },
            _=tokio::time::sleep(delay)=>{},
        }
    }
    // Socket references cannot survive the allocations they borrow.
    drop(sockets);
    drop(tcp_flows);
    Ok(())
}
fn tuple(packet: &[u8]) -> Option<(Protocol, Tuple, bool)> {
    let version = packet.first()? >> 4;
    let (source, destination, protocol, offset) = if version == 4 {
        if packet.len() < 20 {
            return None;
        }
        if u16::from_be_bytes([packet[6], packet[7]]) & 0x1fff != 0 {
            return None;
        }
        (
            IpAddr::V4(Ipv4Addr::new(
                packet[12], packet[13], packet[14], packet[15],
            )),
            IpAddr::V4(Ipv4Addr::new(
                packet[16], packet[17], packet[18], packet[19],
            )),
            packet[9],
            ((packet[0] & 15) as usize) * 4,
        )
    } else if version == 6 {
        if packet.len() < 40 {
            return None;
        }
        (
            IpAddr::V6(std::net::Ipv6Addr::from(
                <[u8; 16]>::try_from(&packet[8..24]).ok()?,
            )),
            IpAddr::V6(std::net::Ipv6Addr::from(
                <[u8; 16]>::try_from(&packet[24..40]).ok()?,
            )),
            packet[6],
            40,
        )
    } else {
        return None;
    };
    let ports = packet.get(offset..offset + 4)?;
    let guest = SocketAddr::new(source, u16::from_be_bytes([ports[0], ports[1]]));
    let remote = SocketAddr::new(destination, u16::from_be_bytes([ports[2], ports[3]]));
    match protocol {
        6 => Some((
            Protocol::Tcp,
            Tuple { guest, remote },
            packet
                .get(offset + 13)
                .is_some_and(|flags| flags & 0x12 == 2),
        )),
        17 => Some((Protocol::Udp, Tuple { guest, remote }, false)),
        _ => None,
    }
}

/// Runner-only relay. TUN and packet directions are independent, so a full
/// packet writer cannot block reads from the other direction or control fd 3.
pub struct Relay {
    incoming: tokio::task::JoinHandle<io::Result<()>>,
    outgoing: tokio::task::JoinHandle<io::Result<()>>,
}
impl Relay {
    pub async fn wait(&mut self) -> io::Result<()> {
        tokio::select! {r=&mut self.incoming=>r.map_err(io::Error::other)?,r=&mut self.outgoing=>r.map_err(io::Error::other)?}
    }
}
impl Drop for Relay {
    fn drop(&mut self) {
        self.incoming.abort();
        self.outgoing.abort();
    }
}
pub fn relay_inherited() -> io::Result<Relay> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let tun = unsafe { std::fs::File::from_raw_fd(5) };
    let socket = unsafe { std::os::unix::net::UnixStream::from_raw_fd(4) };
    socket.set_nonblocking(true)?;
    let flags = unsafe { libc::fcntl(tun.as_raw_fd(), libc::F_GETFL) };
    if unsafe { libc::fcntl(tun.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let tun = std::sync::Arc::new(unsafe { tokio::io::unix::AsyncFd::register(tun) }?);
    let socket = tokio::net::UnixStream::from_std(socket)?;
    let (mut reader, mut writer) = socket.into_split();
    let read_tun = tun.clone();
    let outgoing = tokio::task::spawn_local(async move {
        let mut bytes = [0u8; wire::MTU];
        loop {
            let n = loop {
                let mut ready = read_tun.readable().await?;
                match ready.try_io(|t| {
                    use std::io::Read;
                    t.get_ref().read(&mut bytes)
                }) {
                    Ok(r) => break r?,
                    Err(_) => continue,
                }
            };
            if n == 0 {
                return Ok::<(), io::Error>(());
            }
            wire::write_frame(&mut writer, b'P', &bytes[..n]).await?;
        }
    });
    let incoming = tokio::task::spawn_local(async move {
        loop {
            let packet = wire::read_frame(&mut reader, b'P').await?;
            loop {
                let mut ready = tun.writable().await?;
                match ready.try_io(|t| {
                    use std::io::Write;
                    t.get_ref().write(&packet)
                }) {
                    Ok(Ok(n)) if n == packet.len() => break,
                    Ok(Ok(_)) => return Err(io::Error::other("partial TUN write")),
                    Ok(Err(e)) => return Err(e),
                    Err(_) => continue,
                }
            }
        }
        #[allow(unreachable_code)]
        Ok::<(), io::Error>(())
    });
    Ok(Relay { incoming, outgoing })
}
