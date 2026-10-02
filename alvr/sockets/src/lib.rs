mod control_socket;
mod secure_control_socket;
mod stream_socket;

use alvr_common::{AnyhowToCon, ConResult, ToCon, anyhow::Result, con_bail, info};
use alvr_packets::{ClientControlPacket, ServerControlPacket};
use alvr_session::{DscpTos, SocketBufferConfig, SocketBufferSize, SocketProtocol};
use serde::{Serialize, de::DeserializeOwned};
use socket2::Socket;
use std::{
    marker::PhantomData,
    net::{IpAddr, Ipv4Addr, TcpListener},
    time::Duration,
};

pub use control_socket::*;
pub use secure_control_socket::*;
pub use stream_socket::*;

pub const LOCAL_IP: IpAddr = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
pub const CONTROL_PORT: u16 = 9943;
pub const HANDSHAKE_PACKET_SIZE_BYTES: usize = 56; // this may change in future protocols
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_millis(500);
pub const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(2);

pub const MDNS_SERVICE_TYPE: &str = "_alvr._tcp.local.";
pub const MDNS_PROTOCOL_KEY: &str = "protocol";
pub const MDNS_DEVICE_ID_KEY: &str = "device_id";

pub const WIRED_CLIENT_HOSTNAME: &str = "client.wired";

fn set_socket_buffers(socket: &socket2::Socket, buffer_config: SocketBufferConfig) -> Result<()> {
    info!(
        "Initial socket buffer size: send: {}B, recv: {}B",
        socket.send_buffer_size()?,
        socket.recv_buffer_size()?
    );

    {
        let maybe_size = match buffer_config.send_size_bytes {
            SocketBufferSize::Default => None,
            SocketBufferSize::Maximum => Some(u32::MAX),
            SocketBufferSize::Custom(size) => Some(size),
        };

        if let Some(size) = maybe_size {
            if let Err(e) = socket.set_send_buffer_size(size as usize) {
                info!("Error setting socket send buffer: {e}");
            } else {
                info!(
                    "Set socket send buffer succeeded: {}",
                    socket.send_buffer_size()?
                );
            }
        }
    }

    {
        let maybe_size = match buffer_config.recv_size_bytes {
            SocketBufferSize::Default => None,
            SocketBufferSize::Maximum => Some(u32::MAX),
            SocketBufferSize::Custom(size) => Some(size),
        };

        if let Some(size) = maybe_size {
            if let Err(e) = socket.set_recv_buffer_size(size as usize) {
                info!("Error setting socket recv buffer: {e}");
            } else {
                info!(
                    "Set socket recv buffer succeeded: {}",
                    socket.recv_buffer_size()?
                );
            }
        }
    }

    Ok(())
}

/// The 6-bit DSCP for a settings value, as the DS field byte the socket takes.
///
/// Pulled out of [`set_dscp`] so it can be tested, which is the point: this arithmetic was
/// wrong for as long as it was inline and unexercised. Two defects, both fixed here and in
/// `DropProbability`:
///
/// * `DropProbability` held `0x10` and `0x11` for `Medium`/`High` — hex literals where
///   binary ones were meant. As `u8`, `Medium` was **16**, so OR-ing it in landed on top of
///   the class field.
/// * The drop precedence was never shifted into its own bit. The IETF value for assured
///   forwarding is `class * 8 + drop * 2` (AF11 = 10, AF13 = 14), so the precedence needs a
///   `<< 1`; without it even the `Low` case was one off (9 instead of 10).
///
/// With both fixed, `class` 1–4 and `drop` 1–3 produce the documented DSCP values.
fn dscp_to_tos(dscp: DscpTos) -> u8 {
    // https://en.wikipedia.org/wiki/Differentiated_services
    match dscp {
        DscpTos::BestEffort => 0,
        DscpTos::ClassSelector(precedence) => (precedence & 0b111) << 3,
        DscpTos::AssuredForwarding {
            class,
            drop_probability,
        } => ((class & 0b111) << 3) | ((drop_probability as u8 & 0b11) << 1),
        DscpTos::ExpeditedForwarding => 0b101110,
    }
}

fn set_dscp(socket: &Socket, dscp: Option<DscpTos>) {
    if let Some(dscp) = dscp {
        // `set_tos_v4` takes the DS *field*, i.e. the 6-bit DSCP shifted up by two.
        socket.set_tos_v4((dscp_to_tos(dscp) << 2) as u32).ok();
    }
}

// connect_to_client should be used on the server side.
// At the moment, the TcpListener is implemened on the client side so the API for this function
// is non-standard
// todo: convert to class when storing a TcpListener
pub fn connect_to_client<T: DeserializeOwned>(
    client_ips: Vec<IpAddr>,
    timeout: Duration,
) -> ConResult<(ProtoControlSocket, IpAddr, T)> {
    let (mut control_socket, client_ip) =
        ProtoControlSocket::connect_to(timeout, PeerType::AnyClient(client_ips))?;

    let res = control_socket.recv(timeout)?;

    Ok((control_socket, client_ip, res))
}

pub fn listen_to_server<T: DeserializeOwned>(
    listener_socket: &TcpListener,
    timeout: Duration,
    client_info: &impl Serialize,
) -> ConResult<(ProtoControlSocket, T)> {
    let (mut control_socket, _) =
        ProtoControlSocket::connect_to(timeout, PeerType::Server(listener_socket))?;

    control_socket.send(client_info).to_con()?;

    let config_packet = control_socket.recv(timeout)?;

    Ok((control_socket, config_packet))
}

pub fn send_restart_signal(
    mut control_socket: ProtoControlSocket,
    stream_config_packet: impl Serialize,
) -> ConResult<()> {
    // We must send the config packet before, which will be unused
    control_socket.send(&stream_config_packet).to_con()?;

    control_socket
        .send(&ServerControlPacket::Restarting)
        .to_con()
}

pub struct StreamSocketConfig {
    pub protocol: SocketProtocol,
    pub port: u16,
    pub buffer_config: SocketBufferConfig,
    pub max_packet_size: usize,
    pub dscp: Option<DscpTos>,
}

pub enum ServerConnectionResult {
    Connected(SocketConnection),
    Restarting,
}

pub struct SocketConnection {
    control_socket: ProtoControlSocket,
    stream_socket: StreamSocket,
}

impl SocketConnection {
    // Note: the timeout resets after each internal operation
    pub fn from_client_connection(
        mut control_socket: ProtoControlSocket,
        timeout: Duration,
        stream_config_packet: impl Serialize,
        socket_config: StreamSocketConfig,
    ) -> ConResult<Self> {
        let client_ip = control_socket.inner.peer_addr().to_con()?.ip();

        control_socket.send(&stream_config_packet).to_con()?;

        control_socket
            .send(&ServerControlPacket::StartStream)
            .to_con()?;

        let signal = control_socket.recv(timeout)?;
        if !matches!(signal, ClientControlPacket::StreamReady) {
            con_bail!("Got unexpected packet waiting for stream ack");
        }

        let stream_socket = StreamSocketBuilder::connect_to_client(
            timeout,
            client_ip,
            socket_config.port,
            socket_config.protocol,
            socket_config.dscp,
            socket_config.buffer_config,
            socket_config.max_packet_size,
        )?;

        Ok(Self {
            control_socket,
            stream_socket,
        })
    }

    // Note: the timeout resets after each internal operation
    pub fn from_server_connection(
        mut control_socket: ProtoControlSocket,
        timeout: Duration,
        socket_config: StreamSocketConfig,
    ) -> ConResult<ServerConnectionResult> {
        let server_ip = control_socket.inner.peer_addr().to_con()?.ip();

        match control_socket.recv(timeout)? {
            ServerControlPacket::StartStream => (),
            ServerControlPacket::Restarting => return Ok(ServerConnectionResult::Restarting),
            _ => con_bail!("Got unexpected packet waiting for stream start"),
        }

        let stream_socket_builder = StreamSocketBuilder::listen_for_server(
            timeout,
            socket_config.port,
            socket_config.protocol,
            socket_config.dscp,
            socket_config.buffer_config,
        )
        .to_con()?;

        control_socket
            .send(&ClientControlPacket::StreamReady)
            .to_con()?;

        let stream_socket = stream_socket_builder.accept_from_server(
            server_ip,
            socket_config.port,
            socket_config.max_packet_size,
            timeout,
        )?;

        Ok(ServerConnectionResult::Connected(Self {
            control_socket,
            stream_socket,
        }))
    }

    pub fn request_reliable_stream<T>(&self) -> ConResult<ControlSocketSender<T>> {
        Ok(ControlSocketSender {
            inner: self.control_socket.inner.try_clone().to_con()?,
            buffer: vec![],
            _phantom: PhantomData,
        })
    }

    pub fn subscribe_to_reliable_stream<T>(&self) -> ConResult<ControlSocketReceiver<T>> {
        Ok(ControlSocketReceiver {
            inner: self.control_socket.inner.try_clone().to_con()?,
            buffer: vec![],
            recv_cursor: None,
            _phantom: PhantomData,
        })
    }

    pub fn request_unreliable_stream<T>(&self, stream_id: u16) -> StreamSender<T> {
        self.stream_socket.request_stream(stream_id)
    }

    pub fn subscribe_to_unreliable_stream<T>(
        &mut self,
        stream_id: u16,
        max_concurrent_buffers: usize,
    ) -> StreamReceiver<T> {
        self.stream_socket
            .subscribe_to_stream(stream_id, max_concurrent_buffers)
    }

    pub fn recv_poll(&mut self) -> ConResult<()> {
        self.stream_socket.recv()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alvr_session::DropProbability;

    #[test]
    fn dscp_values_match_the_ietf_classes() {
        assert_eq!(dscp_to_tos(DscpTos::BestEffort), 0);
        assert_eq!(dscp_to_tos(DscpTos::ClassSelector(5)), 40); // CS5
        assert_eq!(dscp_to_tos(DscpTos::ExpeditedForwarding), 46); // EF

        // The three assured-forwarding precedences of class 1: AF11, AF12, AF13.
        for (label, drop, expected) in [
            ("low", DropProbability::Low, 10),
            ("medium", DropProbability::Medium, 12),
            ("high", DropProbability::High, 14),
        ] {
            assert_eq!(
                dscp_to_tos(DscpTos::AssuredForwarding {
                    class: 1,
                    drop_probability: drop,
                }),
                expected,
                "AF1x with {label} precedence"
            );
        }

        assert_eq!(
            dscp_to_tos(DscpTos::AssuredForwarding {
                class: 4,
                drop_probability: DropProbability::High,
            }),
            38,
            "AF43 = 4*8 + 3*2"
        );
    }

    #[test]
    fn the_drop_precedence_never_touches_the_class_bits() {
        // The regression that motivated pulling this out: a precedence value that is not
        // masked into its own bits corrupts the class. Every combination must land on the
        // arithmetic, not merely on something in range.
        for class in 1..=4u8 {
            for (drop, drop_bits) in [
                (DropProbability::Low, 1u8),
                (DropProbability::Medium, 2),
                (DropProbability::High, 3),
            ] {
                let value = dscp_to_tos(DscpTos::AssuredForwarding {
                    class,
                    drop_probability: drop,
                });
                assert_eq!(value, class * 8 + drop_bits * 2);
                assert_eq!(value >> 3, class, "class bits changed by the precedence");
            }
        }
    }

    #[test]
    fn the_ds_field_is_the_dscp_shifted_up_by_two() {
        // `set_tos_v4` wants the field, not the code point: EF (46) becomes 184.
        assert_eq!(
            (dscp_to_tos(DscpTos::ExpeditedForwarding) << 2) as u32,
            0b1011_1000
        );
    }
}
