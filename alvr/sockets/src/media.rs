//! The media plane's datagram plumbing: one UDP socket, as `x-transport`'s two traits.
//!
//! `x-transport` has no sockets on purpose — a plane that knows about `socket2` cannot be driven by a
//! recording or a bench — and that decision left the *binding* homeless, so the receiving half was
//! written in `client_core` and the sending half did not exist. This is the binding, in the crate
//! that already owns socket policy, so both ends use one implementation and neither can drift from
//! the other.
//!
//! ## The timeout, which was wrong
//!
//! The first version of this set the socket non-blocking once, at construction, and then called
//! `set_read_timeout` per read. On Linux those two do not compose: `SO_RCVTIMEO` has no effect on a
//! socket in non-blocking mode, so the read returned `WouldBlock` immediately, every time, and the
//! source would have reported `Timeout` for a socket that had datagrams waiting on it. A receiver fed
//! by that source would have reported a silent link and asked for keyframes forever.
//!
//! Nothing caught it because the trait's contract is "return what is there or say there is nothing",
//! and `Timeout` is a legal answer. The mode is now set per call, from the timeout that was asked
//! for, and there is a test with a real socket that would have failed.

use std::{
    net::{IpAddr, SocketAddr, UdpSocket},
    time::Duration,
};

use alvr_common::{ConResult, ToCon, anyhow::Result};
use alvr_session::DscpTos;
use x_transport::{DatagramSink, DatagramSource, SinkError, SourceEvent};

/// The largest datagram the media plane can be carrying.
///
/// `x-transport` fragments to the negotiated MTU; a datagram bigger than this is not a fragment we
/// can parse, and reading a truncated version of it turns a routing problem into a mysterious parse
/// error. The kernel truncates silently, so a full buffer is indistinguishable from an exactly-full
/// datagram and is refused.
const MAX_DATAGRAM_SIZE: usize = 4096;

/// How long `send` waits for room in a full kernel buffer before giving the datagram up.
///
/// A send that meets a full buffer is backpressure, not an error: the socket is non-blocking
/// because the *receive* path needs it to be (see [`MediaSocket::send`]), and dropping a fragment
/// out of the middle of a frame is a frame that can never be assembled. The limit exists only so a
/// socket that can never drain cannot park the send thread forever.
const SEND_BLOCK_STEP: Duration = Duration::from_micros(250);
const SEND_BLOCK_LIMIT: Duration = Duration::from_millis(50);

/// One media-plane socket: a sink and a source over the same endpoint.
pub struct MediaSocket {
    socket: UdpSocket,
    /// The one peer whose datagrams are the stream. `None` accepts any sender.
    peer: Option<SocketAddr>,
    datagrams_sent: u64,
    send_failures: u64,
    /// How many sends had to wait for room in the kernel buffer. A non-zero value is the link
    /// applying backpressure, which is a fact worth being able to see rather than infer.
    send_waits: u64,
    datagrams_received: u64,
    datagrams_from_elsewhere: u64,
    datagrams_oversized: u64,
    /// The last address a datagram actually came from.
    ///
    /// A receiver that does not know the sender's port — one that bound a well-known port and
    /// accepted the first datagram from anywhere — needs this to reply. The media plane's feedback
    /// goes back the way the stream came, and on a connection built this way that is the only way
    /// to learn where "back" is.
    last_sender: Option<SocketAddr>,
}

impl std::fmt::Debug for MediaSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaSocket")
            .field("local", &self.socket.local_addr().ok())
            .field("peer", &self.peer)
            .field("datagrams_sent", &self.datagrams_sent)
            .field("datagrams_received", &self.datagrams_received)
            .finish()
    }
}

impl MediaSocket {
    /// Bind for receiving, accepting datagrams from anyone. A session knows its peer, so
    /// [`Self::bind_to`] is what the live path uses; this is for a relay or a test.
    pub fn bind(port: u16, dscp: Option<DscpTos>) -> ConResult<Self> {
        let socket = UdpSocket::bind((crate::LOCAL_IP, port)).to_con()?;
        let mut socket = Self::from_socket(socket);
        socket.peer = None;

        // A media socket is the one place the QoS marking matters and the one place it was never
        // checked: see `set_dscp`, which reports why a mark did not take rather than swallowing it.
        let raw: socket2::Socket = socket.socket.try_clone().to_con()?.into();
        if let Some(reason) = crate::set_dscp(&raw, dscp) {
            alvr_common::warn!("{reason}");
        }
        Ok(socket)
    }

    /// Bind for receiving from **one** peer. Datagrams from anywhere else are counted and dropped:
    /// a client that accepts a datagram from anywhere is a client that will believe anything.
    ///
    /// The **host** is pinned, not the port: the peer sends from an ephemeral port, and this end
    /// cannot know it until a datagram arrives ([`MediaSocket::last_sender`]), which
    /// [`MediaSocket::accept_only_from`] pins at that point. The only port available here is this
    /// end's own, and pinning *that* means no datagram ever matches — the socket drains into a
    /// counter and the plane reports an empty link while packets pile up in the kernel buffer. Port
    /// 0 is the encoding for "any port from this host".
    pub fn bind_to(port: u16, peer: IpAddr, dscp: Option<DscpTos>) -> ConResult<Self> {
        let mut socket = Self::bind(port, dscp)?;
        socket.peer = Some(SocketAddr::new(peer, 0));
        Ok(socket)
    }

    /// Open an ephemeral local port to a peer, for the sending end.
    ///
    /// **`local_addr` will report `0.0.0.0:port`, and that is not what the peer will see.** The bind
    /// is deliberately unspecified — a host with several interfaces should let routing choose the
    /// source, and binding one address would pin the session to it — but it means the address this
    /// end reports and the address its datagrams appear to come from are different things. A peer
    /// that needs to reply must learn the source from a datagram it received
    /// ([`MediaSocket::last_sender`]), which is what the client's feedback socket does. Reading
    /// `local_addr` and handing it to the other end produces a socket that sends into the void, and
    /// the failure is a frame that simply never arrives.
    pub fn connect_to(peer: SocketAddr, dscp: Option<DscpTos>) -> ConResult<Self> {
        let socket = UdpSocket::bind((crate::LOCAL_IP, 0)).to_con()?;
        let mut socket = Self::from_socket(socket);
        socket.peer = Some(peer);

        let raw: socket2::Socket = socket.socket.try_clone().to_con()?.into();
        if let Some(reason) = crate::set_dscp(&raw, dscp) {
            alvr_common::warn!("{reason}");
        }
        Ok(socket)
    }

    /// Wrap a socket the caller owns.
    ///
    /// For a caller that has already bound and configured one — the client's session does, because
    /// the port comes from the negotiation — and for tests, which need an ephemeral port and a peer
    /// they choose. With no peer set, datagrams from anywhere are accepted.
    pub fn from_std(socket: UdpSocket) -> ConResult<Self> {
        Ok(Self::from_socket(socket))
    }

    fn from_socket(socket: UdpSocket) -> Self {
        // Blocking with a per-call timeout; see the module docs for why this is not `nonblocking`.
        socket.set_nonblocking(false).ok();
        Self {
            socket,
            peer: None,
            datagrams_sent: 0,
            send_failures: 0,
            send_waits: 0,
            datagrams_received: 0,
            datagrams_from_elsewhere: 0,
            datagrams_oversized: 0,
            last_sender: None,
        }
    }

    pub fn local_addr(&self) -> ConResult<SocketAddr> {
        self.socket.local_addr().to_con()
    }

    pub fn peer(&self) -> Option<SocketAddr> {
        self.peer
    }

    /// A second handle on the same local port, for the other direction.
    ///
    /// A sender and a receiver should not share one object: the send path needs `&mut` for the
    /// whole of a frame's datagrams while the receive path is blocked in `recv_from`, and the
    /// counters for the two directions would be one jumbled number. Each half counts its own
    /// direction, which is also the only way either number means anything.
    pub fn try_clone(&self) -> ConResult<Self> {
        let socket = self.socket.try_clone().to_con()?;
        let mut clone = Self::from_socket(socket);
        clone.peer = self.peer;
        Ok(clone)
    }

    /// Only accept datagrams from this address. A session knows its peer, and a client that accepts
    /// a datagram from anywhere is a client that will believe anything.
    pub fn accept_only_from(&mut self, peer: SocketAddr) {
        self.peer = Some(peer);
    }

    /// The address the last datagram came from, for a receiver that has to reply.
    pub fn last_sender(&self) -> Option<SocketAddr> {
        self.last_sender
    }

    pub fn datagrams_sent(&self) -> u64 {
        self.datagrams_sent
    }

    /// Sends the kernel refused. Non-zero means the caller is not honouring the pacer: the buffer
    /// fills when datagrams leave faster than the link can carry them, and a media plane that
    /// discovers this *after* the frame has been cut up has already spent the frame.
    pub fn send_failures(&self) -> u64 {
        self.send_failures
    }

    /// Sends that had to wait for room in the kernel buffer — the link applying backpressure.
    pub fn send_waits(&self) -> u64 {
        self.send_waits
    }

    pub fn datagrams_received(&self) -> u64 {
        self.datagrams_received
    }

    /// Datagrams from a host that is not the streamer. Non-zero means something else on the network
    /// is talking to this port, which is worth knowing before wondering why frames are corrupt.
    pub fn datagrams_from_elsewhere(&self) -> u64 {
        self.datagrams_from_elsewhere
    }

    pub fn datagrams_oversized(&self) -> u64 {
        self.datagrams_oversized
    }
}

impl DatagramSink for MediaSocket {
    fn send(&mut self, datagram: &[u8]) -> Result<(), SinkError> {
        // A peer pinned by `bind_to` has no port — it is a *receive* filter until the peer's source
        // is learned. Sending to "any port" is meaningless, so it is treated as no peer at all, and
        // the connected send that follows fails loudly rather than going somewhere arbitrary.
        let peer = self.peer.filter(|peer| peer.port() != 0);

        // **Wait for room rather than dropping a datagram.**
        //
        // The receive path puts this socket in non-blocking mode — a poll has to be non-blocking,
        // because `SO_RCVTIMEO` is ignored on one — and the mode is a property of the *socket*, not
        // of this handle: `try_clone` shares it. So a send that is not expecting to be non-blocking
        // meets a full kernel buffer with `WouldBlock`, and a sender that treats that as a refusal
        // drops **one fragment out of the middle of a frame**. The frame can then never be
        // assembled, and it presents as packet loss on a link that is merely busy.
        //
        // Waiting is the backpressure at the right granularity: the caller's bounded video queue
        // then drops whole frames, which is a decision it is able to make and can count.
        let mut waited = Duration::ZERO;
        loop {
            let result = match peer {
                Some(peer) => self.socket.send_to(datagram, peer),
                None => self.socket.send(datagram),
            };

            match result {
                Ok(_) => {
                    self.datagrams_sent += 1;
                    return Ok(());
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        && waited < SEND_BLOCK_LIMIT =>
                {
                    self.send_waits += 1;
                    waited += SEND_BLOCK_STEP;
                    std::thread::sleep(SEND_BLOCK_STEP);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    self.send_failures += 1;
                    return Err(SinkError::WouldBlock);
                }
                Err(_) => return Err(SinkError::Closed),
            }
        }
    }
}

impl DatagramSource for MediaSocket {
    fn recv(&mut self, out: &mut Vec<u8>, timeout: Duration) -> SourceEvent {
        // The mode is set from the timeout that was asked for. The two cannot be set once and
        // forgotten: on Linux `SO_RCVTIMEO` is ignored on a non-blocking socket, so a read would
        // return `WouldBlock` forever and the source would report an empty link.
        if timeout.is_zero() {
            self.socket.set_nonblocking(true).ok();
        } else {
            self.socket.set_nonblocking(false).ok();
            self.socket.set_read_timeout(Some(timeout)).ok();
        }

        let mut buffer = [0u8; MAX_DATAGRAM_SIZE];
        match self.socket.recv_from(&mut buffer) {
            Ok((len, from)) => {
                // A peer with port 0 pins the **host** only — the sender's source port is ephemeral
                // and is learned from the first datagram. `accept_only_from` replaces the peer with
                // an exact address, which then matches on the port as well.
                if let Some(peer) = self.peer
                    && (peer.ip() != from.ip() || (peer.port() != 0 && peer.port() != from.port()))
                {
                    self.datagrams_from_elsewhere += 1;
                    // Reported as a timeout rather than as a datagram: it is not a fragment, and
                    // handing it to the receiver would count it as a rejected one — which is a
                    // different fact about a different problem.
                    return SourceEvent::Timeout;
                }
                if len == buffer.len() {
                    self.datagrams_oversized += 1;
                    return SourceEvent::Timeout;
                }

                self.datagrams_received += 1;
                self.last_sender = Some(from);
                out.clear();
                out.extend_from_slice(&buffer[..len]);
                SourceEvent::Datagram
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                SourceEvent::Timeout
            }
            // A socket that cannot be read from again will never deliver another datagram.
            Err(_) => SourceEvent::Closed,
        }
    }
}

// `send_to` on the *test* sender, which is otherwise connected, so the test reads as what it means.
#[cfg(test)]
impl MediaSocket {
    fn send_to(&mut self, peer: &SocketAddr, datagram: &[u8]) -> Result<(), SinkError> {
        match self.socket.send_to(datagram, peer) {
            Ok(_) => Ok(()),
            Err(_) => Err(SinkError::Closed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ConnectionError` is not `Debug`, so an assertion helper rather than `unwrap`.
    fn ok<T>(result: ConResult<T>, what: &str) -> T {
        match result {
            Ok(value) => value,
            Err(e) => panic!("{what}: {e}"),
        }
    }

    fn pair() -> (MediaSocket, MediaSocket) {
        let receiver = ok(MediaSocket::bind(0, None), "bind receiver");
        let addr = ok(receiver.local_addr(), "local addr");
        let sender = ok(MediaSocket::connect_to(addr, None), "connect sender");
        (receiver, sender)
    }

    /// The defect this module was written to fix, as a test: a datagram that is **there** must be
    /// returned, not reported as a timeout. The first version of this set the socket non-blocking
    /// once and then set a read timeout per call, which is a no-op on a non-blocking socket — so it
    /// answered `Timeout` for a socket with a datagram waiting on it, forever.
    #[test]
    fn a_datagram_that_is_there_is_returned_rather_than_reported_as_a_timeout() {
        let (mut receiver, mut sender) = pair();

        sender.send(b"a frame's first fragment").unwrap();
        assert_eq!(sender.datagrams_sent(), 1);

        let mut out = Vec::new();
        assert_eq!(
            receiver.recv(&mut out, Duration::from_millis(50)),
            SourceEvent::Datagram
        );
        assert_eq!(out, b"a frame's first fragment");
        assert_eq!(receiver.datagrams_received(), 1);
    }

    #[test]
    fn an_empty_socket_times_out() {
        let (mut receiver, _sender) = pair();
        let mut out = Vec::new();
        assert_eq!(
            receiver.recv(&mut out, Duration::from_millis(5)),
            SourceEvent::Timeout
        );
    }

    /// A zero timeout means "poll once", not "block forever" and not "return nothing".
    #[test]
    fn a_zero_timeout_polls_and_still_returns_what_is_there() {
        let (mut receiver, mut sender) = pair();
        sender.send(b"polled").unwrap();

        let mut out = Vec::new();
        assert_eq!(
            receiver.recv(&mut out, Duration::ZERO),
            SourceEvent::Datagram
        );
        assert_eq!(out, b"polled");

        assert_eq!(
            receiver.recv(&mut out, Duration::ZERO),
            SourceEvent::Timeout,
            "and then says there is nothing, without blocking"
        );
    }

    #[test]
    fn a_datagram_from_another_host_is_counted_and_not_handed_over() {
        let (mut receiver, mut sender) = pair();
        // Re-point the receiver at a peer that is not the sender.
        receiver.peer = Some("127.0.0.1:9".parse().unwrap());
        let target = ok(receiver.local_addr(), "local addr");
        sender.send_to(&target, b"not the streamer").unwrap();

        let mut out = Vec::new();
        assert_eq!(
            receiver.recv(&mut out, Duration::from_millis(50)),
            SourceEvent::Timeout
        );
        assert!(out.is_empty());
        assert_eq!(receiver.datagrams_from_elsewhere(), 1);
    }

    /// The defect a live run found, and the reason the whole media plane looked dead while the
    /// kernel's receive queue filled up.
    ///
    /// `bind_to` used to pin the peer to `peer_ip:<this end's own port>`. The peer sends from an
    /// *ephemeral* port — it cannot bind the client's media port, and the client cannot know the
    /// ephemeral one until a datagram arrives — so no datagram ever matched, every one was counted
    /// `from_elsewhere` and dropped, and the plane reported `0 datagrams in (0 dropped, 0 rejected)`
    /// on a link that was delivering perfectly. A black screen that reads as a network fault.
    #[test]
    fn bind_to_pins_the_host_and_not_the_source_port() {
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};

        // A concrete local port, exactly as the client's media port is — not 0. The bug this
        // guards was `peer = peer_ip:<this port>`, and port 0 would accidentally agree with the
        // "any port" encoding and hide it.
        let probe = ok(MediaSocket::bind(0, None), "probe");
        let port = ok(probe.local_addr(), "probe addr").port();
        drop(probe);

        let mut receiver = ok(
            MediaSocket::bind_to(port, IpAddr::V4(Ipv4Addr::LOCALHOST), None),
            "bind_to receiver",
        );
        assert_eq!(
            receiver.peer,
            Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)),
            "the port must not be pinned to this end's own"
        );
        let addr = ok(receiver.local_addr(), "local addr");
        assert_eq!(addr.port(), port);

        // The peer, sending from whatever ephemeral port the kernel gave it.
        let mut sender = ok(MediaSocket::connect_to(addr, None), "connect sender");
        sender.send(b"a fragment from the streamer").unwrap();

        let mut out = Vec::new();
        assert_eq!(
            receiver.recv(&mut out, Duration::from_millis(50)),
            SourceEvent::Datagram,
            "a datagram from the pinned host must be handed over, whatever port it came from"
        );
        assert_eq!(out, b"a fragment from the streamer");
        assert_eq!(receiver.datagrams_from_elsewhere(), 0);
    }

    /// ...and once the source *is* known, the exact address — including its port — is enforced.
    #[test]
    fn accept_only_from_still_matches_on_the_port() {
        let (mut receiver, mut sender) = pair();
        receiver.accept_only_from("127.0.0.1:1".parse().unwrap());

        let target = ok(receiver.local_addr(), "local addr");
        sender.send_to(&target, b"from the wrong port").unwrap();

        let mut out = Vec::new();
        assert_eq!(
            receiver.recv(&mut out, Duration::from_millis(20)),
            SourceEvent::Timeout
        );
        assert_eq!(receiver.datagrams_from_elsewhere(), 1);
    }

    #[test]
    fn a_datagram_as_large_as_the_buffer_is_refused_rather_than_truncated() {
        let (mut receiver, mut sender) = pair();
        sender
            .send(&vec![0u8; MAX_DATAGRAM_SIZE])
            .expect("the send itself is fine; the read is what refuses it");

        let mut out = Vec::new();
        assert_eq!(
            receiver.recv(&mut out, Duration::from_millis(50)),
            SourceEvent::Timeout
        );
        assert_eq!(receiver.datagrams_oversized(), 1);
    }
}
