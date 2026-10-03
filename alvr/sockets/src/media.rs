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

/// One media-plane socket: a sink and a source over the same endpoint.
pub struct MediaSocket {
    socket: UdpSocket,
    /// The one peer whose datagrams are the stream. `None` accepts any sender.
    peer: Option<SocketAddr>,
    datagrams_sent: u64,
    send_failures: u64,
    datagrams_received: u64,
    datagrams_from_elsewhere: u64,
    datagrams_oversized: u64,
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
    pub fn bind_to(port: u16, peer: IpAddr, dscp: Option<DscpTos>) -> ConResult<Self> {
        let mut socket = Self::bind(port, dscp)?;
        socket.peer = Some(SocketAddr::new(peer, port));
        Ok(socket)
    }

    /// Connect an ephemeral local port to a peer, for the sending end.
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
            datagrams_received: 0,
            datagrams_from_elsewhere: 0,
            datagrams_oversized: 0,
        }
    }

    pub fn local_addr(&self) -> ConResult<SocketAddr> {
        self.socket.local_addr().to_con()
    }

    pub fn peer(&self) -> Option<SocketAddr> {
        self.peer
    }

    /// Only accept datagrams from this address. A session knows its peer, and a client that accepts
    /// a datagram from anywhere is a client that will believe anything.
    pub fn accept_only_from(&mut self, peer: SocketAddr) {
        self.peer = Some(peer);
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
        let result = match self.peer {
            Some(peer) => self.socket.send_to(datagram, peer),
            None => self.socket.send(datagram),
        };

        match result {
            Ok(_) => {
                self.datagrams_sent += 1;
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                self.send_failures += 1;
                Err(SinkError::WouldBlock)
            }
            Err(_) => Err(SinkError::Closed),
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
                if self.peer.is_some_and(|peer| peer != from) {
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
