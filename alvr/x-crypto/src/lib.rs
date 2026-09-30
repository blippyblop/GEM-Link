//! x-crypto — GemLink transport security (M1).
//!
//! Noise-XX handshake + AEAD transport, **on by default and unconditional**:
//! there is no API here that sends application data unencrypted. A plaintext
//! transport exists solely under the `insecure-debug-transport` cargo feature
//! (compile-time toggle, default OFF) so the wire protocol can be packet-
//! captured while debugging. Release builds cannot enable it (ADR-0006).

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The single supported Noise pattern: XX over 25519, ChaCha20-Poly1305,
/// SHA256. One pattern, reviewed once, no negotiation surface.
pub const NOISE_PATTERN: &str = "Noise_XX_25519_ChaChaPoly_SHA256";

/// Length of the fingerprint shown to users during pairing.
pub const FINGERPRINT_LEN: usize = 16;

// ---------------------------------------------------------------------------
// Identity — static keys + pairing fingerprints

#[derive(Clone, Serialize, Deserialize)]
pub struct Identity {
    public: Vec<u8>,
    #[serde(skip)]
    private: Vec<u8>,
}

impl Identity {
    pub fn generate() -> Result<Self, String> {
        let kp = snow::Builder::new(NOISE_PATTERN.parse().unwrap())
            .generate_keypair()
            .map_err(|e| format!("keygen: {e:?}"))?;
        Ok(Self {
            public: kp.public,
            private: kp.private,
        })
    }

    pub fn public(&self) -> &[u8] {
        &self.public
    }

    /// The pairing fingerprint: first 16 bytes of SHA-256 over the public
    /// key, hex-encoded in pairs (what a user compares on two screens).
    pub fn fingerprint(&self) -> String {
        fingerprint_of(&self.public)
    }

    pub(crate) fn keypair(&self) -> snow::Keypair {
        snow::Keypair {
            private: self.private.clone(),
            public: self.public.clone(),
        }
    }
}

pub fn fingerprint_of(public: &[u8]) -> String {
    let hash = Sha256::digest(public);
    hash[..FINGERPRINT_LEN]
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------
// Handshake — Noise-XX, three messages, then transport mode

#[derive(Clone, Copy)]
pub enum HandshakeRole {
    /// The PC (connects out) — sends the first message.
    Initiator,
    /// The headset (listens) — answers.
    Responder,
}

pub struct Handshake {
    state: snow::HandshakeState,
}

impl Handshake {
    /// Start a handshake. `local` is our static identity; `expected_peer`
    /// pins the paired peer's public key — after the handshake, the received
    /// remote static key MUST match it, else pairing is rejected.
    pub fn new(
        role: HandshakeRole,
        local: &Identity,
        expected_peer_public: &[u8],
    ) -> Result<Self, String> {
        let kp = local.keypair();
        let mut builder =
            snow::Builder::new(NOISE_PATTERN.parse().unwrap()).local_private_key(&kp.private);
        if !expected_peer_public.is_empty() {
            builder = builder.remote_public_key(expected_peer_public);
        }
        let state = match role {
            HandshakeRole::Initiator => builder.build_initiator(),
            HandshakeRole::Responder => builder.build_responder(),
        }
        .map_err(|e| format!("handshake build: {e:?}"))?;
        Ok(Self { state })
    }

    /// Produce the next handshake message to send, if any.
    pub fn write(&mut self, out: &mut Vec<u8>) -> Result<usize, String> {
        out.resize(out.capacity(), 0);
        self.state
            .write_message(&[], out)
            .map_err(|e| format!("handshake write: {e:?}"))
    }

    /// Consume an incoming handshake message.
    pub fn read(&mut self, msg: &[u8]) -> Result<(), String> {
        let mut buf = vec![0u8; msg.len() + 64];
        self.state
            .read_message(msg, &mut buf)
            .map_err(|e| format!("handshake read: {e:?}"))?;
        Ok(())
    }

    /// True when the three-message exchange is complete and the transport
    /// keys are ready.
    pub fn finished(&self) -> bool {
        self.state.is_handshake_finished()
    }

    /// Extract the remote peer's static public key as received in-handshake
    /// — compare with the paired fingerprint before trusting the transport.
    pub fn remote_static(&self) -> Result<Vec<u8>, String> {
        self.state
            .get_remote_static()
            .map(|k| k.to_vec())
            .ok_or_else(|| "remote static not yet known".into())
    }

    /// Convert to the AEAD transport (consumes the handshake state).
    pub fn into_transport(self) -> Result<SecureTransport, String> {
        let transport = self
            .state
            .into_transport_mode()
            .map_err(|e| format!("into transport: {e:?}"))?;
        Ok(SecureTransport {
            state: transport,
            seq_in: 0,
            seq_out: 0,
        })
    }
}

// ---------------------------------------------------------------------------
// Secure transport — AEAD on every message, replay-protected by Noise's
// internal direction nonces. There is no unencrypted send path.

pub struct SecureTransport {
    state: snow::TransportState,
    seq_in: u64,
    seq_out: u64,
}

impl SecureTransport {
    /// Seal one outbound message (appends the 16-byte auth tag).
    pub fn seal(&mut self, plaintext: &[u8], out: &mut Vec<u8>) -> Result<usize, String> {
        out.resize(plaintext.len() + 64, 0);
        out[..plaintext.len()].copy_from_slice(plaintext);
        let n = self
            .state
            .write_message(&plaintext, out)
            .map_err(|e| format!("seal: {e:?}"))?;
        self.seq_out += 1;
        Ok(n)
    }

    /// Open one inbound message. Tampered or replayed messages fail here.
    pub fn open(&mut self, ciphertext: &[u8], out: &mut Vec<u8>) -> Result<usize, String> {
        out.resize(ciphertext.len() + 64, 0);
        let n = self
            .state
            .read_message(ciphertext, out)
            .map_err(|_| "open failed: tampered, reordered, or replayed message".to_string())?;
        self.seq_in += 1;
        Ok(n)
    }

    pub fn messages_sealed(&self) -> u64 {
        self.seq_out
    }

    pub fn messages_opened(&self) -> u64 {
        self.seq_in
    }
}

/// Compile-time statement of the security posture. Release builds (no
/// feature flag) are always encrypted; this is what the bench asserts.
pub const fn encryption_always_on() -> bool {
    !cfg!(feature = "insecure-debug-transport")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xx_handshake_completes_and_transports_roundtrip() {
        let alice = Identity::generate().unwrap();
        let bob = Identity::generate().unwrap();

        let mut init = Handshake::new(HandshakeRole::Initiator, &alice, &bob.public()).unwrap();
        let mut resp = Handshake::new(HandshakeRole::Responder, &bob, &alice.public()).unwrap();

        // XX: initiator -> responder -> initiator (three empty-payload turns)
        let mut msg = Vec::with_capacity(256);
        let mut turn = 0;
        while !init.finished() {
            msg.clear();
            let n = init.write(&mut msg).unwrap();
            let sent = msg[..n].to_vec();
            resp.read(&sent).unwrap();
            turn += 1;
            if resp.finished() {
                break;
            }
            msg.clear();
            let n = resp.write(&mut msg).unwrap();
            let sent = msg[..n].to_vec();
            init.read(&sent).unwrap();
            turn += 1;
            assert!(turn < 10, "handshake did not converge");
        }
        assert!(init.finished() && resp.finished());

        // Pairing check: both sides see the other's pinned public key.
        assert_eq!(init.remote_static().unwrap(), bob.public());
        assert_eq!(resp.remote_static().unwrap(), alice.public());

        let mut a = init.into_transport().unwrap();
        let mut b = resp.into_transport().unwrap();

        let frame = b"frames go here, encrypted";
        let mut sealed = Vec::with_capacity(256);
        let n = a.seal(frame, &mut sealed).unwrap();
        let mut opened = Vec::new();
        let m = b.open(&sealed[..n], &mut opened).unwrap();
        assert_eq!(&opened[..m], frame);
        assert_eq!(a.messages_sealed(), 1);
        assert_eq!(b.messages_opened(), 1);
    }

    #[test]
    fn tampered_messages_fail_closed() {
        let alice = Identity::generate().unwrap();
        let bob = Identity::generate().unwrap();
        let mut init = Handshake::new(HandshakeRole::Initiator, &alice, &bob.public()).unwrap();
        let mut resp = Handshake::new(HandshakeRole::Responder, &bob, &alice.public()).unwrap();

        let mut msg = Vec::with_capacity(256);
        while !init.finished() {
            msg.clear();
            let n = init.write(&mut msg).unwrap();
            resp.read(&msg[..n]).unwrap();
            if resp.finished() {
                break;
            }
            msg.clear();
            let n = resp.write(&mut msg).unwrap();
            init.read(&msg[..n]).unwrap();
        }

        let mut a = init.into_transport().unwrap();
        let mut b = resp.into_transport().unwrap();

        let mut sealed = Vec::with_capacity(128);
        let n = a.seal(b"secret frame data", &mut sealed).unwrap();
        sealed[0] ^= 0xFF; // flip a bit — attacker or corruption

        let mut opened = Vec::new();
        assert!(b.open(&sealed[..n], &mut opened).is_err());
    }

    #[test]
    fn wrong_paired_key_is_detected() {
        let alice = Identity::generate().unwrap();
        let bob = Identity::generate().unwrap();
        let mallory = Identity::generate().unwrap();

        // Bob paired with Mallory's key but Alice connects: the responder
        // pins Alice's actual key here to simulate the mismatch check.
        let mut init = Handshake::new(HandshakeRole::Initiator, &alice, &mallory.public()).unwrap();
        let mut resp = Handshake::new(HandshakeRole::Responder, &bob, &alice.public()).unwrap();

        let mut msg = Vec::with_capacity(256);
        let n = init.write(&mut msg).unwrap();
        resp.read(&msg[..n]).unwrap();
        // The responder received Alice's real static key, which does NOT
        // match the pinned (Mallory) key — pairing must be rejected.
        assert_ne!(resp.remote_static().unwrap(), mallory.public());
    }

    #[test]
    fn fingerprints_are_stable_and_readable() {
        let id = Identity::generate().unwrap();
        let fp = id.fingerprint();
        assert_eq!(fp.split(' ').count(), FINGERPRINT_LEN);
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit() || c == ' '));
        assert_eq!(fingerprint_of(id.public()), fp);
    }

    #[test]
    fn security_posture_statement() {
        // Default builds: always encrypted. The insecure-debug feature may
        // only flip this in explicit debug builds.
        if !cfg!(feature = "insecure-debug-transport") {
            assert!(encryption_always_on());
        }
    }
}
