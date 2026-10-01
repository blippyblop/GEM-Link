//! Length-prefixed Noise-XX framing — the wire format for encrypted control
//! frames. Mirrors upstream `alvr_sockets` framing conventions (u32-LE
//! length prefix, little-endian, bincode payloads live one layer up) so the
//! encrypted path is a drop-in for the plaintext one:
//!
//! ```text
//! plaintext frame:   [u32-LE n][n bytes bincode]
//! encrypted frame:   [u32-LE n][n bytes = plaintext || 16-byte AEAD tag]
//! ```
//!
//! The handshake itself uses the same framing, so a connection is one
//! consistent length-prefixed byte stream from SYN to close. `recv_frame`
//! honors the same contract as upstream's `framed_recv`: a deadline, not a
//! blocking-forever read, with partial-read accumulation across calls.

use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use crate::{Handshake, HandshakeRole, Identity};

/// Hard cap on any single frame (prefix is untrusted input — this is the
/// allocation-DoS guard). Control-plane frames are orders of magnitude
/// smaller; media chunks stay well under this too.
pub const MAX_FRAME_LEN: u32 = 64 * 1024 * 1024;

pub const HEADER_LEN: usize = 4;

const HANDSHAKE_READ_TIMEOUT: Duration = Duration::from_secs(10);

pub struct NoiseSocket {
    stream: TcpStream,
    transport: crate::SecureTransport,
    // Inbound accumulation state — survives partial reads across calls.
    header: [u8; HEADER_LEN],
    header_have: usize,
    body: Vec<u8>,
    body_have: usize,
}

impl NoiseSocket {
    /// Complete the Noise-XX handshake over length-prefixed framing. Works
    /// for both roles via a converging loop: attempt a write when the
    /// pattern expects one, otherwise read. Returns the transport-mode
    /// socket and the remote peer's static public key (compare its
    /// fingerprint against the pairing store BEFORE trusting frames).
    pub fn handshake(
        stream: TcpStream,
        role: HandshakeRole,
        identity: &Identity,
    ) -> Result<(Self, Vec<u8>), String> {
        let mut stream = stream;
        stream
            .set_read_timeout(Some(HANDSHAKE_READ_TIMEOUT))
            .map_err(|e| format!("handshake: set_read_timeout: {e}"))?;
        let mut hs = Handshake::new(role, identity, &[])?;
        let mut msg = [0u8; 128];
        loop {
            if hs.finished() {
                break;
            }
            // Write turn? (Errors here mean the pattern expects a read.)
            if let Ok(n) = hs.write(&mut msg)
                && n > 0
            {
                write_frame(&mut stream, &msg[..n])?;
                continue;
            }
            let frame = read_frame_blocking(&mut stream)?;
            hs.read(&frame)?;
        }
        let remote_static = hs.remote_static()?;
        let transport = hs.into_transport()?;
        Ok((
            Self {
                stream,
                transport,
                header: [0u8; HEADER_LEN],
                header_have: 0,
                body: Vec::new(),
                body_have: 0,
            },
            remote_static,
        ))
    }

    /// Seal and send one frame. No plaintext send path exists.
    pub fn send_frame(&mut self, plaintext: &[u8]) -> Result<(), String> {
        let mut ct = vec![0u8; plaintext.len() + 64];
        let n = self.transport.seal(plaintext, &mut ct)?;
        write_frame(&mut self.stream, &ct[..n])
    }

    /// Receive and open one frame. Honors the deadline like upstream's
    /// `framed_recv`; partial reads accumulate, so a frame interrupted by
    /// a timeout continues on the next call.
    pub fn recv_frame(&mut self, timeout: Duration) -> Result<Vec<u8>, String> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.header_have < HEADER_LEN {
                let n = read_some(
                    &mut self.stream,
                    &mut self.header[self.header_have..],
                    deadline,
                )?;
                self.header_have += n;
                continue;
            }
            if self.body.is_empty() {
                let len = u32::from_le_bytes(self.header);
                if len > MAX_FRAME_LEN {
                    return Err(format!(
                        "recv_frame: declared frame {len} exceeds cap {MAX_FRAME_LEN}"
                    ));
                }
                self.body = vec![0u8; len as usize];
                self.body_have = 0;
            }
            if self.body_have < self.body.len() {
                let n = read_some(&mut self.stream, &mut self.body[self.body_have..], deadline)?;
                self.body_have += n;
                continue;
            }
            let mut plaintext = vec![0u8; self.body.len()];
            let n = self.transport.open(&self.body, &mut plaintext)?;
            plaintext.truncate(n);
            self.header_have = 0;
            self.body = Vec::new();
            self.body_have = 0;
            return Ok(plaintext);
        }
    }

    /// Seal without framing — ciphertext+tag bytes for callers that embed
    /// Noise frames in their own container. Shares cipher state with
    /// `send_frame` (the two MUST NOT be interleaved carelessly).
    pub fn seal_raw(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, String> {
        let mut ct = vec![0u8; plaintext.len() + 64];
        let n = self.transport.seal(plaintext, &mut ct)?;
        ct.truncate(n);
        Ok(ct)
    }

    /// Recover the inner stream (e.g. to reject a peer after a failed
    /// pairing check without leaking the socket).
    pub fn into_inner(self) -> TcpStream {
        self.stream
    }

    pub fn messages_sealed(&self) -> u64 {
        self.transport.messages_sealed()
    }

    pub fn messages_opened(&self) -> u64 {
        self.transport.messages_opened()
    }
}

fn write_frame<W: Write>(stream: &mut W, payload: &[u8]) -> Result<(), String> {
    if payload.len() > MAX_FRAME_LEN as usize {
        return Err(format!(
            "send_frame: payload {} exceeds cap {MAX_FRAME_LEN}",
            payload.len()
        ));
    }
    let header = (payload.len() as u32).to_le_bytes();
    stream
        .write_all(&header)
        .and_then(|_| stream.write_all(payload))
        .map_err(|e| format!("send_frame: write: {e}"))
}

fn read_some<S: Read>(stream: &mut S, buf: &mut [u8], deadline: Instant) -> Result<usize, String> {
    loop {
        match stream.read(buf) {
            Ok(0) => return Err("recv_frame: connection closed".into()),
            Ok(n) => return Ok(n),
            Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
                if Instant::now() > deadline {
                    return Err("recv_frame: deadline exceeded".into());
                }
                // Brief park so a quiet peer doesn't burn the core.
                std::thread::sleep(Duration::from_micros(200));
            }
            Err(e) => return Err(format!("recv_frame: read: {e}")),
        }
    }
}

fn read_frame_blocking(stream: &mut TcpStream) -> Result<Vec<u8>, String> {
    let mut header = [0u8; HEADER_LEN];
    read_exact_with_deadline(stream, &mut header, Instant::now() + HANDSHAKE_READ_TIMEOUT)?;
    let len = u32::from_le_bytes(header);
    if len > MAX_FRAME_LEN {
        return Err(format!(
            "handshake: declared frame {len} exceeds cap {MAX_FRAME_LEN}"
        ));
    }
    let mut body = vec![0u8; len as usize];
    read_exact_with_deadline(stream, &mut body, Instant::now() + HANDSHAKE_READ_TIMEOUT)?;
    Ok(body)
}

fn read_exact_with_deadline(
    stream: &mut TcpStream,
    buf: &mut [u8],
    deadline: Instant,
) -> Result<(), String> {
    let mut have = 0;
    while have < buf.len() {
        match stream.read(&mut buf[have..]) {
            Ok(0) => return Err("handshake: connection closed".into()),
            Ok(n) => have += n,
            Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
                if Instant::now() > deadline {
                    return Err("handshake: read deadline exceeded".into());
                }
            }
            Err(e) => return Err(format!("handshake: read: {e}")),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn loopback_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let client = TcpStream::connect(addr).expect("connect");
        let (server, _) = listener.accept().expect("accept");
        client.set_nodelay(true).expect("nodelay");
        server.set_nodelay(true).expect("nodelay");
        (client, server)
    }

    fn handshake_pair() -> (NoiseSocket, NoiseSocket) {
        let (c, s) = loopback_pair();
        let client_id = Identity::generate().expect("id");
        let server_id = Identity::generate().expect("id");
        let server_pub = server_id.public().to_vec();
        // Server side must run concurrently — XX has the client write first.
        let server_join = std::thread::spawn(move || {
            NoiseSocket::handshake(s, HandshakeRole::Responder, &server_id)
                .expect("server handshake")
        });
        let (client, c_remote) = NoiseSocket::handshake(c, HandshakeRole::Initiator, &client_id)
            .expect("client handshake");
        let (server, s_remote) = server_join.join().expect("server thread");
        // XX: each side's remote static is the OTHER peer's identity key.
        assert_eq!(c_remote, server_pub, "client saw server key");
        assert_eq!(s_remote, client_id.public(), "server saw client key");
        (client, server)
    }

    fn assert_err(result: Result<Vec<u8>, String>, needle: &str) {
        match result {
            Ok(_) => panic!("expected error containing {needle:?}"),
            Err(e) => assert!(e.contains(needle), "got: {e}"),
        }
    }

    #[test]
    fn framed_roundtrip_echo_preserves_order_and_counters() {
        let (mut client, mut server) = handshake_pair();
        for i in 0..50u32 {
            let msg = format!("control-frame-{i}");
            client.send_frame(msg.as_bytes()).expect("send");
            let got = server.recv_frame(Duration::from_secs(2)).expect("srv recv");
            assert_eq!(got, msg.as_bytes());
            let ack = format!("ack-{i}");
            server.send_frame(ack.as_bytes()).expect("server send");
            let got = client.recv_frame(Duration::from_secs(2)).expect("recv");
            assert_eq!(got, ack.as_bytes());
        }
        assert_eq!(client.messages_sealed(), 50);
        assert_eq!(client.messages_opened(), 50);
        assert_eq!(server.messages_sealed(), 50);
        assert_eq!(server.messages_opened(), 50);
    }

    #[test]
    fn oversized_declared_frame_is_rejected_before_allocation() {
        let (client, mut server) = handshake_pair();

        // Legit peer hands the raw stream back; we then speak like an
        // attacker: declare a frame larger than the cap.
        let mut raw = client.into_inner();
        let hostile_len = MAX_FRAME_LEN + 1;
        raw.write_all(&hostile_len.to_le_bytes())
            .expect("write len");

        let err = server.recv_frame(Duration::from_secs(2));
        assert_err(err, "exceeds cap");
    }

    #[test]
    fn foreign_session_frame_fails_closed() {
        // Two independent sessions; feed session-B ciphertext to session A.
        let (a, mut b) = handshake_pair();
        let (_a2, mut b2) = handshake_pair();

        let foreign = b2.seal_raw(b"smuggled").expect("seal foreign");

        let mut raw = a.into_inner();
        raw.write_all(&(foreign.len() as u32).to_le_bytes())
            .expect("len");
        raw.write_all(&foreign).expect("body");

        let err = b.recv_frame(Duration::from_secs(2));
        // Either the AEAD open fails outright, or (timestamp edge) the
        // deadline trips first — both are fails-closed outcomes.
        match err {
            Ok(_) => panic!("foreign ciphertext must not open"),
            Err(e) => assert!(
                e.contains("open failed") || e.contains("deadline"),
                "got: {e}"
            ),
        }
    }

    #[test]
    fn handshake_rejects_oversized_first_frame() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let attacker = TcpStream::connect(addr).expect("connect");
        attacker.set_nodelay(true).expect("nodelay");
        let (victim, _) = listener.accept().expect("accept");
        victim.set_nodelay(true).expect("nodelay");

        let mut evil = attacker;
        evil.write_all(&(MAX_FRAME_LEN + 1).to_le_bytes())
            .expect("write");

        let result = NoiseSocket::handshake(
            victim,
            HandshakeRole::Responder,
            &Identity::generate().expect("id"),
        );
        match result {
            Ok(_) => panic!("oversized handshake frame must be rejected"),
            Err(e) => assert!(e.contains("exceeds cap"), "got: {e}"),
        }
    }
}
