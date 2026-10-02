//! Noise-XX secured control socket — drop-in secure sibling of
//! `control_socket`. Composition strategy: the Noise cipher state cannot be
//! split into directional halves (single snow TransportState), so the
//! socket holds ONE NoiseSocket behind a mutex and send/recv serialize on
//! it. Control traffic is request/response and low-rate — the lock is
//! uncontended in practice and bounds no hot path (media flows on the
//! stream socket, see ADR-0006 for posture).

use crate::CONTROL_PORT;
use alvr_common::{AnyhowToCon, ConResult, HandleTryAgain, ToCon, anyhow::Result, con_bail};
use alvr_session::SocketBufferConfig;
use bincode::config;
use serde::{Serialize, de::DeserializeOwned};
use std::{
    net::{IpAddr, SocketAddr, TcpListener, TcpStream},
    sync::Mutex,
    time::Duration,
};
use x_crypto::framed::NoiseSocket;
use x_crypto::{HandshakeRole, Identity, fingerprint_of};

pub struct SecureControlSocket {
    inner: Mutex<NoiseSocket>,
}

impl SecureControlSocket {
    /// Server side: accept the client's TCP connection, then run the
    /// Noise-XX handshake as responder. Mirrors `accept_from_server`.
    /// Returns the socket plus the client's static public key — the caller
    /// MUST verify `fingerprint_of(&remote_static)` against the pairing
    /// store before trusting the session.
    pub fn accept_from_client(
        listener: &TcpListener,
        client_ip: Option<IpAddr>,
        timeout: Duration,
        identity: &Identity,
    ) -> ConResult<(Self, Vec<u8>)> {
        let (socket, client_address) = listener.accept().handle_try_again()?;

        if let Some(ip) = client_ip
            && client_address.ip() != ip
        {
            con_bail!(
                "Connected to wrong client: Expected: {ip}, Found {}",
                client_address.ip()
            );
        }

        socket.set_read_timeout(Some(timeout)).to_con()?;
        socket.set_nodelay(true).to_con()?;

        Self::handshake(socket, HandshakeRole::Responder, identity)
    }

    /// Client side: connect to the server, then run the Noise-XX handshake
    /// as initiator. Mirrors `connect_to_client` (per-IP split timeout).
    pub fn connect_to_server(
        timeout: Duration,
        server_ips: &[IpAddr],
        port: u16,
        buffer_config: SocketBufferConfig,
        identity: &Identity,
    ) -> ConResult<(Self, Vec<u8>)> {
        let split_timeout = timeout / server_ips.len().max(1) as u32;

        let mut res = alvr_common::try_again();
        for ip in server_ips {
            res = TcpStream::connect_timeout(&SocketAddr::new(*ip, port), split_timeout)
                .handle_try_again();

            if res.is_ok() {
                break;
            }
        }
        let socket = res?.into();

        crate::set_socket_buffers(&socket, buffer_config).ok();
        socket.set_read_timeout(Some(timeout)).to_con()?;

        let socket = TcpStream::from(socket);

        socket.set_nodelay(true).to_con()?;

        Self::handshake(socket, HandshakeRole::Initiator, identity)
    }

    /// Convenience for the common well-known-port case.
    pub fn connect_to_server_default(
        timeout: Duration,
        server_ips: &[IpAddr],
        identity: &Identity,
    ) -> ConResult<(Self, Vec<u8>)> {
        let buffer = SocketBufferConfig::default();
        Self::connect_to_server(timeout, server_ips, CONTROL_PORT, buffer, identity)
    }

    fn handshake(
        socket: TcpStream,
        role: HandshakeRole,
        identity: &Identity,
    ) -> ConResult<(Self, Vec<u8>)> {
        let (noise, remote_static) = NoiseSocket::handshake(socket, role, identity)
            .map_err(|e| alvr_common::anyhow::anyhow!("Noise handshake failed: {e}"))
            .to_con()?;
        Ok((
            Self {
                inner: Mutex::new(noise),
            },
            remote_static,
        ))
    }

    /// Verify the peer's fingerprint against an expected value. Call this
    /// before the first exchange; pairing-pin mismatch is fatal.
    pub fn verify_remote(&self, remote_static: &[u8], expected_fingerprint: &str) -> ConResult<()> {
        if fingerprint_of(remote_static) != expected_fingerprint {
            con_bail!(
                "Pairing fingerprint mismatch: expected {expected_fingerprint}, got {}",
                fingerprint_of(remote_static)
            );
        }
        Ok(())
    }

    /// Seal, frame, and send one typed control packet. No plaintext path.
    pub fn send<S: Serialize>(&self, packet: &S) -> Result<()> {
        let mut noise = self
            .inner
            .lock()
            .map_err(|e| alvr_common::anyhow::anyhow!("secure control socket poisoned: {e}"))?;
        let mut payload = Vec::new();
        bincode::serde::encode_into_std_write(packet, &mut payload, config::standard())?;
        noise
            .send_frame(&payload)
            .map_err(|e| alvr_common::anyhow::anyhow!("secure send: {e}"))
    }

    /// Receive, open, and decode one typed control packet. Honors the
    /// deadline with upstream's try-again semantics; a partially-received
    /// frame resumes on the next call (accumulation lives in NoiseSocket).
    pub fn recv<R: DeserializeOwned>(&self, timeout: Duration) -> ConResult<R> {
        let mut noise = self
            .inner
            .lock()
            .map_err(|e| alvr_common::anyhow::anyhow!("secure control socket poisoned: {e}"))
            .to_con()?;

        let payload = match noise.recv_frame(timeout) {
            Ok(p) => p,
            Err(e) if e.contains("deadline") => return alvr_common::try_again(),
            Err(e) => {
                return Err(alvr_common::ConnectionError::Other(
                    alvr_common::anyhow::anyhow!("secure recv: {e}"),
                ));
            }
        };

        let (packet, _) =
            bincode::serde::decode_from_slice(&payload, config::standard()).to_con()?;

        Ok(packet)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alvr_common::ConnectionError;
    use alvr_packets::{ClientControlPacket, ServerControlPacket};

    type StrResult<T> = std::result::Result<T, String>;

    fn secured_pair() -> StrResult<(SecureControlSocket, SecureControlSocket)> {
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
        let addr = listener.local_addr().map_err(|e| e.to_string())?;

        let server_id = Identity::generate()?;
        let server_pub = server_id.public().to_vec();
        let server_handle = std::thread::spawn(move || {
            SecureControlSocket::accept_from_client(
                &listener,
                None,
                Duration::from_secs(5),
                &server_id,
            )
            .map_err(|e| e.to_string())
        });

        let client_id = Identity::generate()?;
        let (client, client_remote) = SecureControlSocket::connect_to_server(
            Duration::from_secs(5),
            &[addr.ip()],
            addr.port(),
            SocketBufferConfig::default(),
            &client_id,
        )
        .map_err(|e| e.to_string())?;
        let (server, server_remote) = server_handle.join().expect("server thread")?;

        // XX: each side saw the other's static key.
        assert_eq!(client_remote, server_pub);
        assert_eq!(server_remote, client_id.public());
        Ok((client, server))
    }

    #[test]
    fn secure_control_typed_roundtrip() {
        let (client, server) = secured_pair().expect("pair");

        let out = ClientControlPacket::KeepAlive;
        client.send(&out).expect("send");
        let got: ClientControlPacket = server
            .recv(Duration::from_secs(2))
            .map_err(|e| e.to_string())
            .expect("recv");
        assert!(matches!(got, ClientControlPacket::KeepAlive));

        let reply = ServerControlPacket::Reserved("ack".into());
        server.send(&reply).expect("server send");
        let got: ServerControlPacket = client
            .recv(Duration::from_secs(2))
            .map_err(|e| e.to_string())
            .expect("recv");
        assert!(matches!(got, ServerControlPacket::Reserved(_)));
    }

    #[test]
    fn secure_control_recv_deadline_is_try_again() {
        let (client, server) = secured_pair().expect("pair");
        let _ = client;
        let start = std::time::Instant::now();
        let res: ConResult<ClientControlPacket> = server.recv(Duration::from_millis(300));
        match res {
            Err(ConnectionError::TryAgain(_)) => {}
            Err(ConnectionError::Other(e)) => panic!("expected TryAgain, got Other: {e:?}"),
            Ok(_) => panic!("expected timeout"),
        }
        assert!(start.elapsed() >= Duration::from_millis(250));
    }

    #[test]
    fn secure_control_fingerprint_mismatch_is_fatal() {
        let (client, _server) = secured_pair().expect("pair");
        let res = client.verify_remote(&[0u8; 32], "0000000000000000");
        assert!(res.is_err());
    }
}
