//! The session key the media plane is built on.
//!
//! Everything in `x-transport` is keyed by a 32-byte secret: the per-datagram AEAD, the key
//! rotation, and the feedback channel. Until now the live path had **no** secret, so the media
//! plane ran in the clear and a NACK rode unauthenticated — 12 bytes asking the server for 1.4 KB, a
//! free amplifier for anyone who could reach the port.
//!
//! `x-crypto` has had a complete Noise-XX stack all along (`Identity`, `Handshake`,
//! `SecureTransport`, pairing fingerprints). What it did not have was an **exporter**, so a key
//! could be derived from the session for a channel that is not the session. It does now, and this is
//! where the derivation is driven between the two ends.
//!
//! ## Where the handshake runs, and why there
//!
//! On the **control socket**: it is TCP, framed, ordered, and already carries the stream
//! configuration. A Noise handshake needs a reliable ordered channel — its messages are three and
//! each depends on the last — and the media socket is the one place that cannot offer that. This is
//! the ordinary shape (key agreement on the reliable channel, the lossy channel keyed from it).
//!
//! ## What this does and does not buy, stated rather than implied
//!
//! **Does:** the media and feedback keys are never transmitted. Both ends derive them from a
//! Diffie-Hellman exchange, so an eavesdropper on the control socket — which is still plaintext
//! today — cannot read or forge a media datagram or a NACK. That is the whole of the amplification
//! problem above, gone.
//!
//! **Does not:** authenticate the peer. The identities here are generated per session and the peer's
//! static key is not pinned, so an **active** man in the middle can complete the handshake with each
//! end separately. Closing that needs pairing — a persistent identity on each end and a fingerprint
//! the user compares — and `x-crypto` already has every primitive for it (`Identity` is
//! `Serialize`, `fingerprint_of` is written for exactly that screen). It is a product decision, not
//! a cryptography one, and this module is where it lands: pass a pinned peer key to
//! [`MediaKeyExchange::with_pinned_peer`] and the TOFU window closes.

use alvr_common::anyhow::{Result, anyhow};
use x_crypto::{Handshake, HandshakeRole, Identity};

/// The key the feedback channel uses. Distinct from every media key, so a NACK cannot be replayed
/// as a video datagram or the other way round.
pub const FEEDBACK_LABEL: &[u8] = b"gemlink/feedback/v1";

/// The key the video datagrams use.
pub const MEDIA_LABEL: &[u8] = b"gemlink/media/v1";

/// Which end of the exchange this is.
///
/// The server initiates, because it is the end that makes the outgoing connection — the same reason
/// the reference client's `SVLDataLink` treats the PC as the connecting side. It is a convention
/// rather than a requirement, but it has to be *a* convention and both ends have to hold the same
/// one, so it is written down here once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKeyRole {
    Server,
    Client,
}

/// The step the exchange is on. Tracked explicitly so a misuse is an error rather than a hang: a
/// handshake driver that reads when the peer is reading blocks forever, and "the session never
/// starts" is a much worse failure than "the driver was called out of order".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// Nothing exchanged yet. The server sends the first message; the client waits.
    Start,
    /// One message in. The server has sent `-> e`; the client has answered `<- e, ee, s, es`.
    One,
    /// Two in. The server has sent `-> s, se`; both ends hold the handshake hash.
    Two,
}

/// Drives a Noise-XX handshake between the two ends of a session and exports the media keys.
pub struct MediaKeyExchange {
    handshake: Handshake,
    role: MediaKeyRole,
    step: Step,
    /// The peer's static key, if this end was told to expect one.
    expected_peer: Option<Vec<u8>>,
}

impl MediaKeyExchange {
    /// An exchange with a fresh ephemeral identity — trust on first use.
    pub fn new(role: MediaKeyRole) -> Result<Self> {
        Self::with_identity(role, &Identity::generate().map_err(|e| anyhow!("{e}"))?)
    }

    /// An exchange that will **only** accept the peer it was told to expect.
    ///
    /// This is the pairing path: `expected_peer` is the public key a user confirmed out of band.
    /// Empty means trust on first use, which is what the live path does today.
    pub fn with_pinned_peer(role: MediaKeyRole, expected_peer: &[u8]) -> Result<Self> {
        Self::with_identity_and_peer(
            role,
            &Identity::generate().map_err(|e| anyhow!("{e}"))?,
            expected_peer,
        )
    }

    fn with_identity(role: MediaKeyRole, identity: &Identity) -> Result<Self> {
        Self::with_identity_and_peer(role, identity, &[])
    }

    fn with_identity_and_peer(
        role: MediaKeyRole,
        identity: &Identity,
        expected_peer: &[u8],
    ) -> Result<Self> {
        let noise_role = match role {
            MediaKeyRole::Server => HandshakeRole::Initiator,
            MediaKeyRole::Client => HandshakeRole::Responder,
        };

        // The peer key is **not** passed to `snow`, and that is deliberate rather than an
        // oversight. `snow` does not enforce `remote_public_key` for XX in either direction:
        // measured, a full handshake between two parties completes when the initiator was given a
        // key belonging to somebody else. Relying on it would mean a pin that silently does
        // nothing, so the check is made here, where it is one line and has a test that fails if it
        // is removed.
        let handshake = Handshake::new(noise_role, identity, &[]).map_err(|e| anyhow!("{e}"))?;

        Ok(Self {
            handshake,
            role,
            step: Step::Start,
            expected_peer: (!expected_peer.is_empty()).then(|| expected_peer.to_vec()),
        })
    }

    /// Reject the peer if it is not the one this end was told to expect.
    ///
    /// Run after every message, because XX reveals the peer's identity at different points for the
    /// two roles — the initiator learns it in message two, the responder in message three — and a
    /// check that only runs at the end is a check that runs after both ends have already derived a
    /// key from a session with the wrong party.
    fn enforce_pin(&self) -> Result<()> {
        let Some(expected) = &self.expected_peer else {
            return Ok(());
        };
        let Ok(received) = self.handshake.remote_static() else {
            // Not known yet. The handshake has not carried an identity, so there is nothing to
            // check and nothing to trust.
            return Ok(());
        };

        if received != *expected {
            return Err(anyhow!(
                "the peer is not the one this session was paired with; refusing the session before                  any key is derived from it"
            ));
        }
        Ok(())
    }

    /// The message this end must send first, if it has one.
    ///
    /// The server does, the client does not — XX opens with the initiator. Returning a bool rather
    /// than making the caller know which end it is keeps that fact in one place.
    pub fn start(&mut self) -> Result<Option<Vec<u8>>> {
        if self.step != Step::Start {
            return Err(anyhow!("the exchange has already started"));
        }

        match self.role {
            MediaKeyRole::Client => Ok(None),
            MediaKeyRole::Server => {
                let message = self.write()?;
                self.step = Step::One;
                Ok(Some(message))
            }
        }
    }

    /// Take an incoming message and produce the reply, if one is due.
    ///
    /// Returns `None` when the exchange is complete and this end owes nothing more.
    pub fn advance(&mut self, incoming: &[u8]) -> Result<Option<Vec<u8>>> {
        if self.is_finished() {
            return Err(anyhow!(
                "the exchange has already finished; another message is either a replay or a peer \
                 that has lost track"
            ));
        }

        let expected = match (self.role, self.step) {
            (MediaKeyRole::Client, Step::Start) => 1,
            (MediaKeyRole::Server, Step::One) => 2,
            (MediaKeyRole::Client, Step::One) => 3,
            _ => return Err(anyhow!("the exchange was called out of order")),
        };
        let _ = expected;

        self.handshake.read(incoming).map_err(|e| anyhow!("{e}"))?;
        self.enforce_pin()?;

        let reply = match (self.role, self.step) {
            // The client has just taken `-> e` and owes `<- e, ee, s, es`.
            (MediaKeyRole::Client, Step::Start) => {
                let message = self.write()?;
                self.step = Step::One;
                Some(message)
            }
            // The server has just taken `<- e, ee, s, es` and owes `-> s, se`.
            (MediaKeyRole::Server, Step::One) => {
                let message = self.write()?;
                self.step = Step::Two;
                Some(message)
            }
            // The client has just taken `-> s, se`. Its half of the handshake is complete.
            (MediaKeyRole::Client, Step::One) => {
                self.step = Step::Two;
                None
            }
            _ => return Err(anyhow!("the exchange was called out of order")),
        };

        Ok(reply)
    }

    fn write(&mut self) -> Result<Vec<u8>> {
        // XX messages carry no payload; snow's bound is a small constant and 256 is comfortably
        // past it for a 25519 pattern.
        let mut buffer = [0u8; 256];
        let len = self
            .handshake
            .write(&mut buffer)
            .map_err(|e| anyhow!("{e}"))?;
        Ok(buffer[..len].to_vec())
    }

    pub fn is_finished(&self) -> bool {
        self.handshake.finished()
    }

    /// The 32-byte secret the media plane is built on.
    ///
    /// Only after [`Self::is_finished`]. An unfinished handshake has an incomplete hash, and two
    /// ends baking different ones derive different keys with nothing to tell them so.
    pub fn session_secret(&self) -> Result<[u8; 32]> {
        self.handshake
            .exporter(MEDIA_LABEL)
            .map_err(|e| anyhow!("{e}"))
    }

    /// The key the feedback channel uses. Derived from the same session, separated by label.
    pub fn feedback_secret(&self) -> Result<[u8; 32]> {
        self.handshake
            .exporter(FEEDBACK_LABEL)
            .map_err(|e| anyhow!("{e}"))
    }

    /// The peer's static public key, for a caller that wants to show or pin a fingerprint.
    pub fn remote_static(&self) -> Result<Vec<u8>> {
        self.handshake.remote_static().map_err(|e| anyhow!("{e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run both ends of an exchange in memory, the way the two processes run it.
    fn exchange() -> (MediaKeyExchange, MediaKeyExchange) {
        let mut server = MediaKeyExchange::new(MediaKeyRole::Server).unwrap();
        let mut client = MediaKeyExchange::new(MediaKeyRole::Client).unwrap();

        // The server speaks first; the client waits, which is what a TCP peer does.
        let mut message = server
            .start()
            .unwrap()
            .expect("the server opens the exchange");
        assert!(client.start().unwrap().is_none());

        // `<- e, ee, s, es`
        message = client
            .advance(&message)
            .unwrap()
            .expect("the client answers");
        // `-> s, se`
        message = server
            .advance(&message)
            .unwrap()
            .expect("the server closes");
        // And the client takes it, owing nothing.
        assert!(client.advance(&message).unwrap().is_none());

        (server, client)
    }

    #[test]
    fn both_ends_agree_on_the_session_secret_without_it_crossing_the_wire() {
        let (server, client) = exchange();

        assert!(server.is_finished() && client.is_finished());
        assert_eq!(
            server.session_secret().unwrap(),
            client.session_secret().unwrap()
        );
        assert_eq!(
            server.feedback_secret().unwrap(),
            client.feedback_secret().unwrap()
        );
    }

    #[test]
    fn the_feedback_key_is_not_the_media_key() {
        let (server, _) = exchange();
        assert_ne!(
            server.session_secret().unwrap(),
            server.feedback_secret().unwrap(),
            "a NACK must not be openable as a video datagram, or the other way round"
        );
    }

    #[test]
    fn two_sessions_share_nothing() {
        let (first, _) = exchange();
        let (second, _) = exchange();

        assert_ne!(
            first.session_secret().unwrap(),
            second.session_secret().unwrap(),
            "ephemeral identities per session: if two sessions derived the same key, the exchange \
             would not be doing anything"
        );
    }

    /// A message that is not a handshake message at all must fail closed.
    ///
    /// Note the shape of this test, because the obvious version of it is *wrong*: feeding one
    /// session's first message to another session does not fail, and should not — XX's opening
    /// message is an ephemeral public key with nothing in it to authenticate yet. What authenticates
    /// is the static key, which arrives two messages later, and that is what
    /// [`a_pinned_peer_is_enforced`] drives. A test that asserted the earlier failure would be
    /// asserting that `snow` accepts degenerate curve points, not that this module works.
    #[test]
    fn a_garbled_message_fails_closed() {
        fn client() -> MediaKeyExchange {
            MediaKeyExchange::new(MediaKeyRole::Client).unwrap()
        }

        // Length alone is not a rejection criterion: a Noise message may carry a payload, so a
        // long message is legal. What is not legal is a message that does not parse as one.
        for garbage in [vec![0u8; 31], vec![0xffu8; 7], Vec::new()] {
            let mut exchange = client();
            assert!(
                exchange.advance(&garbage).is_err(),
                "a {}-byte message that is not a handshake must be refused, not carried forward",
                garbage.len()
            );
            assert!(!exchange.is_finished());
        }
    }

    #[test]
    fn calling_the_exchange_out_of_order_is_an_error_and_not_a_hang() {
        let mut client = MediaKeyExchange::new(MediaKeyRole::Client).unwrap();
        // The client owes nothing until the server's first message arrives.
        assert!(client.start().unwrap().is_none());

        let mut server = MediaKeyExchange::new(MediaKeyRole::Server).unwrap();
        let first = server.start().unwrap().unwrap();

        // Advancing the server, which owes a *read* rather than a write.
        assert!(
            server.advance(&first).is_err(),
            "the server has not received anything yet"
        );

        // And a finished exchange refuses more.
        let mut message = client.advance(&first).unwrap().unwrap();
        message = server.advance(&message).unwrap().unwrap();
        assert!(client.advance(&message).unwrap().is_none());
        assert!(
            client.advance(&message).is_err(),
            "a message after the handshake is a replay or a confused peer, and neither should be \
             fed to snow"
        );
    }

    #[test]
    fn a_pinned_peer_is_enforced() {
        // The **initiator** is the end that verifies a peer's identity first: XX's second message
        // carries the responder's static key, encrypted, and `snow` checks it against the pinned one
        // as it arrives. So a server told to expect one client meets a different one and stops.
        let mut server =
            MediaKeyExchange::with_pinned_peer(MediaKeyRole::Server, &[7u8; 32]).unwrap();
        let mut client = MediaKeyExchange::new(MediaKeyRole::Client).unwrap();

        let mut message = server.start().unwrap().unwrap();
        message = client.advance(&message).unwrap().unwrap();

        assert!(
            server.advance(&message).is_err(),
            "pinning exists so a user-confirmed key closes the trust-on-first-use window, and the \
             check has to happen at the message that actually carries the identity"
        );
        assert!(!server.is_finished());
        assert!(
            server.session_secret().is_err(),
            "a refused handshake must not hand out a key"
        );
    }

    #[test]
    fn a_peer_that_was_pinned_correctly_completes() {
        // The two ends, with the peer key known in advance — the pairing path.
        let server_identity = Identity::generate().unwrap();
        let client_identity = Identity::generate().unwrap();

        let mut server = MediaKeyExchange::with_identity_and_peer(
            MediaKeyRole::Server,
            &server_identity,
            client_identity.public(),
        )
        .unwrap();
        let mut client = MediaKeyExchange::with_identity_and_peer(
            MediaKeyRole::Client,
            &client_identity,
            server_identity.public(),
        )
        .unwrap();

        let mut message = server.start().unwrap().unwrap();
        message = client.advance(&message).unwrap().unwrap();
        message = server.advance(&message).unwrap().unwrap();
        assert!(client.advance(&message).unwrap().is_none());

        assert_eq!(
            server.session_secret().unwrap(),
            client.session_secret().unwrap()
        );
        assert_eq!(
            client.remote_static().unwrap(),
            server_identity.public(),
            "the received static key is what a user would compare against the fingerprint"
        );
    }
}
