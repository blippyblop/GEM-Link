//! Per-datagram AEAD, with nonces derived rather than counted.
//!
//! ADR-0006 makes encryption default-on end to end; the control plane gets it from
//! [`x_crypto`]'s Noise handshake, and this is the media plane's half. Two things about it
//! are not obvious and are the whole reason it is its own module.
//!
//! ## 1. A counter nonce would desynchronise on the first lost datagram
//!
//! The obvious implementation is a ChaCha20-Poly1305 state with an incrementing nonce,
//! which is what `snow`'s `CipherState` gives you and what the control socket uses. On a
//! TCP stream that is correct. **On a lossy datagram plane it is fatal**: one dropped
//! datagram leaves the sender's counter one ahead of the receiver's, and every subsequent
//! datagram fails to authenticate. The stream would appear to die at the first loss.
//!
//! So the nonce is **derived** from the datagram's own identity — `frame_index` and
//! `fragment_index` — which makes sealing and opening stateless, order-independent and
//! loss-tolerant. That also removes M1's design trap: there is no shared cipher state, so
//! there is nothing to put a `Mutex` around, and the media plane never contends with the
//! control plane.
//!
//! ## 2. This is why the monotonic frame index is load-bearing for security
//!
//! ChaCha20-Poly1305 is catastrophically broken by nonce reuse under the same key. Deriving
//! the nonce from `(frame_index, fragment_index)` is only safe because `frame_index` is
//! **monotonic for the life of the key** — which is exactly the signal ADR-0011 requires for
//! an unrelated reason (telling the receiver a frame went missing). The same field is doing
//! diagnostics and nonce uniqueness.
//!
//! That makes a key's lifetime the thing that has to be right: **a new key per session**, and
//! never a key that outlives a frame-index reset. If the sender's frame index ever restarts
//! under the same key, this is broken — so the key is derived per session and the index is
//! never reset within one.
//!
//! ## 3. A key that never rotates is a key whose nonce space is bounded by luck
//!
//! The sentence above — "never a key that outlives a frame-index reset" — is a *requirement*, and
//! until [`KeySchedule`] existed nothing enforced it. A session that ran long enough to run out of
//! frame index, or a sender that restarted its index after a reconnect without a new key, would
//! repeat nonces under one key. That is not a theoretical worry: `FrameScheduler` and the trust gate
//! both expect a reconnecting sender, and the reference client treats key lifetime as an operational
//! quantity (`SVLDataLink::InitCrypt() With %lu remaining key max` — a budget, not a constant).
//!
//! So keys are derived from a session secret and tied to a **key epoch**, which rides on the wire
//! and is authenticated with everything else. Rotation needs no exchange: both ends derive the same
//! key from the same secret and the same epoch. See [`KeySchedule`] for why the epoch is what the
//! sender rotates on and not simply the frame index.
//!
//! ## What this does not do
//!
//! **It does not stop replay.** A replayed datagram carries a valid nonce and a valid tag,
//! so it authenticates; catching it is the receiver's job, by remembering which fragments it
//! has already seen ([`crate::receiver`] counts duplicates for exactly this reason). An AEAD
//! is integrity and confidentiality, not freshness — worth stating because "it's
//! authenticated" is often read as "it's replay-proof".

use chacha20poly1305::{
    ChaCha20Poly1305, Nonce, Tag,
    aead::{AeadInPlace, KeyInit},
};
use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Key length, bytes.
pub const KEY_LEN: usize = 32;
/// Authentication tag length, bytes.
pub const TAG_LEN: usize = 16;
/// Nonce length, bytes.
pub const NONCE_LEN: usize = 12;

/// Why a datagram could not be sealed or opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CryptoError {
    /// The tag did not verify: wrong key, tampered ciphertext, or tampered associated data.
    /// Deliberately one variant — distinguishing them is a gift to an attacker and useless
    /// to us.
    Authentication,
    /// Shorter than a tag, so there is nothing to authenticate.
    TooShort,
    /// The output buffer was too small.
    OutputTooSmall,
}

impl std::fmt::Display for CryptoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CryptoError::Authentication => f.write_str("media datagram failed authentication"),
            CryptoError::TooShort => f.write_str("media datagram shorter than its tag"),
            CryptoError::OutputTooSmall => f.write_str("output buffer too small"),
        }
    }
}

impl std::error::Error for CryptoError {}

/// A media-plane cipher for one key.
///
/// Not `Clone` on purpose: one instance, one key, and duplicating it is how the same
/// `(frame_index, fragment_index)` gets sealed twice under one key by two threads.
pub struct MediaCipher {
    cipher: ChaCha20Poly1305,
}

impl std::fmt::Debug for MediaCipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print key material, even in a panic message.
        f.write_str("MediaCipher(<key redacted>)")
    }
}

impl MediaCipher {
    /// Build from a session key. The key must be unique to the session: see the module docs
    /// on nonce reuse.
    pub fn new(key: &[u8; KEY_LEN]) -> Self {
        Self {
            cipher: ChaCha20Poly1305::new(key.into()),
        }
    }

    /// The nonce for one fragment of one frame.
    ///
    /// 8 bytes of frame index, 2 of fragment index, 2 of zero. Uniqueness rests on the frame
    /// index being monotonic and `fragment_index` fitting in 16 bits, which the wire format
    /// already enforces.
    pub const fn nonce(frame_index: u64, fragment_index: u16) -> [u8; NONCE_LEN] {
        let f = frame_index.to_le_bytes();
        let g = fragment_index.to_le_bytes();
        [
            f[0], f[1], f[2], f[3], f[4], f[5], f[6], f[7], g[0], g[1], 0, 0,
        ]
    }

    /// Seal in place *after* appending the tag: returns the datagram length.
    ///
    /// `associated_data` is the datagram header ([`crate::wire`]), so every header field is
    /// authenticated. That matters: without it, an attacker who could not forge the payload
    /// could still relabel the frame index or the fragment index, and both are
    /// load-bearing.
    pub fn seal_into(
        &self,
        frame_index: u64,
        fragment_index: u16,
        associated_data: &[u8],
        plaintext: &[u8],
        out: &mut [u8],
    ) -> Result<usize, CryptoError> {
        let needed = plaintext.len() + TAG_LEN;
        if out.len() < needed {
            return Err(CryptoError::OutputTooSmall);
        }
        out[..plaintext.len()].copy_from_slice(plaintext);
        let nonce = Self::nonce(frame_index, fragment_index);
        let tag = self
            .cipher
            .encrypt_in_place_detached(
                Nonce::from_slice(&nonce),
                associated_data,
                &mut out[..plaintext.len()],
            )
            .map_err(|_| CryptoError::Authentication)?;
        out[plaintext.len()..needed].copy_from_slice(&tag);
        Ok(needed)
    }

    /// Seal into a fresh buffer. For tests and for callers that are not on the hot path.
    pub fn seal(
        &self,
        frame_index: u64,
        fragment_index: u16,
        associated_data: &[u8],
        plaintext: &[u8],
    ) -> Result<Vec<u8>, CryptoError> {
        let mut out = vec![0u8; plaintext.len() + TAG_LEN];
        let len = self.seal_into(
            frame_index,
            fragment_index,
            associated_data,
            plaintext,
            &mut out,
        )?;
        out.truncate(len);
        Ok(out)
    }

    /// Open, returning the plaintext. `attached` is the on-wire payload (ciphertext ‖ tag).
    pub fn open(
        &self,
        frame_index: u64,
        fragment_index: u16,
        associated_data: &[u8],
        attached: &[u8],
    ) -> Result<Vec<u8>, CryptoError> {
        if attached.len() < TAG_LEN {
            return Err(CryptoError::TooShort);
        }
        let split = attached.len() - TAG_LEN;
        let mut body = attached[..split].to_vec();
        let tag = Tag::from_slice(&attached[split..]);
        let nonce = Self::nonce(frame_index, fragment_index);

        self.cipher
            .decrypt_in_place_detached(Nonce::from_slice(&nonce), associated_data, &mut body, tag)
            .map_err(|_| CryptoError::Authentication)?;
        Ok(body)
    }
}

/// How many frames one derived key covers before the sender moves to the next epoch.
///
/// A judgement, stated as one: at 90 Hz this is **one minute of stream per key**, which bounds the
/// nonce space at 60 * 90 * shards. The nonce carries a 64-bit frame index and a 16-bit fragment
/// index, so this is nowhere near a collision risk on its own — the reason to rotate at all is that
/// a *reset* frame index under one key is a repeat, and a minute is short enough that a session which
/// reconnects repeatedly still changes keys often, and long enough that the derivation is not on any
/// hot path.
pub const FRAMES_PER_KEY: u64 = 90 * 60;

/// The domain separator. Distinct per purpose, so a future KDF for something else cannot collide
/// with this one.
const KEY_DERIVATION_CONTEXT: &[u8] = b"gemlink/media-key/v1";

/// The domain separator for the feedback channel. Distinct from every media key.
const FEEDBACK_KEY_CONTEXT: &[u8] = b"gemlink/feedback-key/v1";

/// Derives media keys from one session secret, one per epoch.
///
/// # Why an epoch and not the frame index
///
/// Deriving a key per frame would work and would be pointless: the derivation would be on the hot
/// path of every datagram, and a receiver would have to derive a key before it could authenticate
/// the datagram that tells it which key to use. An **epoch** is the smallest thing that can be
/// carried in a header and understood without a lookup, it rotates on a schedule the sender alone
/// decides, and the receiver needs to hold at most the current and the previous key — which is
/// exactly the tolerance a lossy link needs, because a datagram sealed under the old key may still
/// be in flight when the new one starts.
///
/// # Why HMAC and not a bare hash
///
/// `HMAC-SHA256(secret, context ‖ epoch)` is a PRF with a proof, and it is the standard "expand" step
/// of every KDF in use. A bare `SHA256(secret ‖ epoch)` would also be fine *given* a uniformly
/// random secret, but it is fine for a reason that has to be argued each time it is read, and the
/// argument fails the moment someone passes a password. The secret here comes from a Noise
/// handshake, so it is uniform — but the construction should not depend on knowing that.
#[derive(Clone)]
pub struct KeySchedule {
    secret: [u8; KEY_LEN],
    frames_per_key: u64,
}

impl std::fmt::Debug for KeySchedule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print key material, even in a panic message. The test below caught the derived
        // `Debug` doing exactly that — which is the argument for having the test rather than a
        // convention.
        write!(
            f,
            "KeySchedule(<secret redacted>, {} frames per key)",
            self.frames_per_key
        )
    }
}

impl KeySchedule {
    /// `secret` must be uniformly random. The module docs explain why the whole scheme rests on it.
    ///
    /// The secret is not printed by `Debug`, for the same reason [`MediaCipher`] is not.
    pub fn new(secret: [u8; KEY_LEN]) -> Self {
        Self {
            secret,
            frames_per_key: FRAMES_PER_KEY,
        }
    }

    /// A schedule with a non-default rotation cadence. For tests and for a link whose budget is
    /// short; production uses [`FRAMES_PER_KEY`].
    pub fn with_frames_per_key(secret: [u8; KEY_LEN], frames_per_key: u64) -> Self {
        Self {
            secret,
            frames_per_key: frames_per_key.max(1),
        }
    }

    pub fn frames_per_key(&self) -> u64 {
        self.frames_per_key
    }

    /// The epoch a frame index belongs to. Epoch 0 is the first key of the session.
    ///
    /// `saturating` rather than wrapping: a frame index that has run past `u16::MAX` epochs is a
    /// session that has been running for eight years at 90 Hz, and wrapping would silently reuse key
    /// 0 — the one failure this whole type exists to prevent.
    pub fn epoch_for(&self, frame_index: u64) -> u16 {
        (frame_index / self.frames_per_key).min(u16::MAX as u64) as u16
    }

    /// The key for an epoch. Deterministic on both ends.
    pub fn key_for(&self, epoch: u16) -> [u8; KEY_LEN] {
        let mut mac = <HmacSha256 as Mac>::new_from_slice(&self.secret)
            .expect("HMAC accepts a key of any length, including this one");
        mac.update(KEY_DERIVATION_CONTEXT);
        mac.update(&epoch.to_be_bytes());
        let out = mac.finalize().into_bytes();

        let mut key = [0u8; KEY_LEN];
        key.copy_from_slice(&out);
        key
    }

    /// The cipher for an epoch.
    ///
    /// Deriving a cipher is not free, so a caller on a hot path should hold the one it is using and
    /// re-derive only when [`Self::epoch_for`] changes — which is the shape [`KeyRing`] exists to
    /// express for the receiving side.
    pub fn cipher_for(&self, epoch: u16) -> MediaCipher {
        MediaCipher::new(&self.key_for(epoch))
    }

    /// The key for the **feedback channel**, domain-separated from every media key.
    ///
    /// It has to be a different key, and not for tidiness. A media datagram's nonce is
    /// `(frame_index, fragment_index)`; a feedback message carries a frame index but no fragment
    /// index, and the sending ends are different — so reusing one key would put two different
    /// messages under one nonce the first time an attacker (or a bug) lined the numbers up, which is
    /// the one thing ChaCha20-Poly1305 does not survive. The context string is what makes the
    /// separation a property of the construction rather than of a caller remembering to pass
    /// different bytes.
    pub fn feedback_key(&self) -> [u8; KEY_LEN] {
        let mut mac = <HmacSha256 as Mac>::new_from_slice(&self.secret)
            .expect("HMAC accepts a key of any length, including this one");
        mac.update(FEEDBACK_KEY_CONTEXT);
        let out = mac.finalize().into_bytes();

        let mut key = [0u8; KEY_LEN];
        key.copy_from_slice(&out);
        key
    }

    /// A cipher for the feedback channel. See [`Self::feedback_key`].
    pub fn cipher_for_feedback(&self) -> MediaCipher {
        MediaCipher::new(&self.feedback_key())
    }
}

/// The receiver's view: the current key, and the one before it.
///
/// A datagram sealed under the previous key can still be in flight when the sender rotates, and on a
/// lossy link it can be in flight for a while. Holding one key back is the smallest tolerance that
/// cannot lose a frame for a reason that is purely about key timing; holding more would be holding
/// keys that a compromised client could be forced to use.
#[derive(Debug)]
pub struct KeyRing {
    schedule: KeySchedule,
    current: (u16, MediaCipher),
    previous: Option<(u16, MediaCipher)>,
}

impl KeyRing {
    pub fn new(schedule: KeySchedule) -> Self {
        let cipher = schedule.cipher_for(0);
        Self {
            schedule,
            current: (0, cipher),
            previous: None,
        }
    }

    pub fn current_epoch(&self) -> u16 {
        self.current.0
    }

    /// The cipher for a datagram's epoch, if this ring knows it.
    ///
    /// Learning a *newer* epoch advances the ring; learning an older one that is not the previous is
    /// refused, because a datagram from three epochs ago is either a very old straggler or a replay,
    /// and neither is worth deriving a key for.
    pub fn cipher_for(&mut self, epoch: u16) -> Option<&MediaCipher> {
        let known =
            epoch == self.current.0 || self.previous.as_ref().is_some_and(|(e, _)| *e == epoch);

        if !known {
            if epoch <= self.current.0 {
                // Older than the previous key: either a very old straggler or a replay. Deriving a
                // key for it would be doing work on behalf of an attacker.
                return None;
            }
            let cipher = self.schedule.cipher_for(epoch);
            let old = std::mem::replace(&mut self.current, (epoch, cipher));
            self.previous = Some(old);
        }

        if epoch == self.current.0 {
            Some(&self.current.1)
        } else {
            self.previous.as_ref().map(|(_, cipher)| cipher)
        }
    }

    /// Whether this ring would accept a datagram of this epoch, without changing it.
    pub fn accepts(&self, epoch: u16) -> bool {
        epoch == self.current.0
            || self.previous.as_ref().is_some_and(|(e, _)| *e == epoch)
            || epoch == self.current.0 + 1
    }
}

/// Which key a receiver uses for a datagram, and how that changes.
///
/// Two shapes rather than one, because the two callers want genuinely different things: a bench or a
/// test wants one key and no derivation, and a live session wants a schedule keyed by the epoch on
/// the wire. Making the fixed case construct a schedule would put a KDF on the bench's hot path to
/// prove nothing.
#[derive(Debug)]
pub enum MediaKeys {
    /// One key for the whole session.
    Fixed(MediaCipher),
    /// A rotating schedule, keyed by the epoch the datagram carries.
    ///
    /// The `epoch` argument comes from the datagram header, which is **authenticated with the
    /// payload** — so a datagram cannot be relabelled into an epoch whose key would open it, because
    /// the relabelling changes the associated data and the tag then fails. That is what makes it safe
    /// to let a field an attacker can see choose which key is tried.
    Ring(KeyRing),
}

impl MediaKeys {
    pub fn fixed(cipher: MediaCipher) -> Self {
        Self::Fixed(cipher)
    }

    pub fn rotating(schedule: KeySchedule) -> Self {
        Self::Ring(KeyRing::new(schedule))
    }

    /// Open one datagram, choosing the key from the epoch it carries.
    pub fn open(
        &mut self,
        epoch: u16,
        frame_index: u64,
        fragment_index: u16,
        associated_data: &[u8],
        attached: &[u8],
    ) -> Result<Vec<u8>, CryptoError> {
        match self {
            MediaKeys::Fixed(cipher) => {
                cipher.open(frame_index, fragment_index, associated_data, attached)
            }
            MediaKeys::Ring(ring) => {
                let Some(cipher) = ring.cipher_for(epoch) else {
                    // An epoch the ring does not hold: a very old straggler, or a replay. Reported
                    // as an authentication failure and not as "unknown epoch", because telling an
                    // attacker which of the two it was is a gift and we would not use the answer.
                    return Err(CryptoError::Authentication);
                };
                cipher.open(frame_index, fragment_index, associated_data, attached)
            }
        }
    }
}

impl From<MediaCipher> for MediaKeys {
    fn from(cipher: MediaCipher) -> Self {
        Self::Fixed(cipher)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(seed: u8) -> [u8; KEY_LEN] {
        let mut k = [0u8; KEY_LEN];
        for (i, byte) in k.iter_mut().enumerate() {
            *byte = seed.wrapping_add(i as u8);
        }
        k
    }

    #[test]
    fn round_trips() {
        let cipher = MediaCipher::new(&key(1));
        let plaintext = b"a frame fragment";
        let aad = [7u8; crate::wire::HEADER_LEN];

        let sealed = cipher.seal(42, 3, &aad, plaintext).unwrap();
        assert_eq!(sealed.len(), plaintext.len() + TAG_LEN);
        assert_ne!(
            &sealed[..plaintext.len()],
            plaintext,
            "plaintext is on the wire"
        );

        let opened = cipher.open(42, 3, &aad, &sealed).unwrap();
        assert_eq!(opened, plaintext);
    }

    #[test]
    fn nonces_are_unique_per_frame_and_fragment() {
        let a = MediaCipher::nonce(1, 0);
        let b = MediaCipher::nonce(1, 1);
        let c = MediaCipher::nonce(2, 0);
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_ne!(b, c);
        // The frame index change must move bytes that a fragment index cannot.
        assert_ne!(
            MediaCipher::nonce(u32::MAX as u64 + 1, 0),
            MediaCipher::nonce(0, 0)
        );
    }

    #[test]
    fn opening_out_of_order_and_with_gaps_works() {
        // The property a counter nonce cannot have, and the reason this module exists.
        let cipher = MediaCipher::new(&key(2));
        let aad = [0u8; crate::wire::HEADER_LEN];
        let sealed: Vec<Vec<u8>> = (0..5)
            .map(|i| {
                cipher
                    .seal(10, i, &aad, format!("fragment {i}").as_bytes())
                    .unwrap()
            })
            .collect();

        // 0 and 1 never arrive; 4 arrives first.
        assert_eq!(cipher.open(10, 4, &aad, &sealed[4]).unwrap(), b"fragment 4");
        assert_eq!(cipher.open(10, 2, &aad, &sealed[2]).unwrap(), b"fragment 2");
        assert_eq!(cipher.open(10, 3, &aad, &sealed[3]).unwrap(), b"fragment 3");
    }

    #[test]
    fn a_tampered_payload_fails() {
        let cipher = MediaCipher::new(&key(3));
        let aad = [0u8; crate::wire::HEADER_LEN];
        let mut sealed = cipher.seal(1, 1, &aad, b"payload").unwrap();
        sealed[0] ^= 0x01;
        assert_eq!(
            cipher.open(1, 1, &aad, &sealed),
            Err(CryptoError::Authentication)
        );
    }

    #[test]
    fn a_tampered_header_fails() {
        // The header is associated data, so relabelling a frame must not be possible even
        // though the header is sent in clear.
        let cipher = MediaCipher::new(&key(4));
        let mut aad = [0u8; crate::wire::HEADER_LEN];
        let sealed = cipher.seal(5, 2, &aad, b"payload").unwrap();
        aad[0] ^= 0xff; // "frame_index" byte
        assert_eq!(
            cipher.open(5, 2, &aad, &sealed),
            Err(CryptoError::Authentication)
        );
    }

    #[test]
    fn a_wrong_key_fails() {
        let aad = [0u8; crate::wire::HEADER_LEN];
        let sealed = MediaCipher::new(&key(5))
            .seal(1, 0, &aad, b"payload")
            .unwrap();
        assert_eq!(
            MediaCipher::new(&key(6)).open(1, 0, &aad, &sealed),
            Err(CryptoError::Authentication)
        );
    }

    #[test]
    fn a_short_datagram_is_rejected_before_decrypting() {
        let cipher = MediaCipher::new(&key(7));
        assert_eq!(
            cipher.open(0, 0, &[], &[0u8; TAG_LEN - 1]),
            Err(CryptoError::TooShort)
        );
    }

    #[test]
    fn seal_into_refuses_a_small_buffer() {
        let cipher = MediaCipher::new(&key(8));
        let mut out = [0u8; 4];
        assert_eq!(
            cipher.seal_into(0, 0, &[], b"0123456789", &mut out),
            Err(CryptoError::OutputTooSmall)
        );
    }

    #[test]
    fn a_retransmit_reseals_identically() {
        // Same (frame, fragment) and same plaintext must produce the same ciphertext, so a
        // retransmit can be answered from a stored datagram rather than re-encrypted.
        let cipher = MediaCipher::new(&key(9));
        let aad = [1u8; crate::wire::HEADER_LEN];
        let first = cipher.seal(3, 4, &aad, b"payload").unwrap();
        let again = cipher.seal(3, 4, &aad, b"payload").unwrap();
        assert_eq!(first, again);
    }

    // -- key rotation -------------------------------------------------------------------------

    fn schedule() -> KeySchedule {
        KeySchedule::with_frames_per_key(key(20), 100)
    }

    #[test]
    fn the_epoch_follows_the_frame_index_and_saturates_rather_than_wrapping() {
        let schedule = schedule();

        assert_eq!(schedule.epoch_for(0), 0);
        assert_eq!(schedule.epoch_for(99), 0);
        assert_eq!(schedule.epoch_for(100), 1);
        assert_eq!(schedule.epoch_for(250), 2);

        // A frame index that has run past every epoch is a session eight years long. Wrapping would
        // silently reuse key 0 — the one failure this type exists to prevent — so it saturates.
        assert_eq!(
            schedule.epoch_for(u64::MAX),
            u16::MAX,
            "the epoch wrapped instead of saturating"
        );
    }

    #[test]
    fn both_ends_derive_the_same_key_from_the_same_secret_and_epoch() {
        let sender = schedule();
        let receiver = schedule();

        for epoch in [0u16, 1, 7, u16::MAX] {
            assert_eq!(sender.key_for(epoch), receiver.key_for(epoch));
        }
    }

    #[test]
    fn a_different_epoch_is_a_different_key_and_a_different_secret_is_a_different_key() {
        let schedule = schedule();

        assert_ne!(schedule.key_for(0), schedule.key_for(1));
        assert_ne!(schedule.key_for(1), schedule.key_for(2));

        let other = KeySchedule::with_frames_per_key(key(21), 100);
        assert_ne!(
            schedule.key_for(0),
            other.key_for(0),
            "two sessions must not derive the same key"
        );
    }

    #[test]
    fn a_datagram_sealed_under_one_epoch_does_not_open_under_another() {
        let schedule = schedule();
        let aad = [0u8; crate::wire::HEADER_LEN];

        let sealed = schedule.cipher_for(1).seal(500, 3, &aad, b"frame").unwrap();

        assert!(schedule.cipher_for(1).open(500, 3, &aad, &sealed).is_ok());
        assert_eq!(
            schedule.cipher_for(2).open(500, 3, &aad, &sealed),
            Err(CryptoError::Authentication),
            "a datagram must not open under the neighbouring epoch's key"
        );
    }

    #[test]
    fn the_ring_keeps_one_key_back_so_a_straggler_still_opens() {
        let mut ring = KeyRing::new(schedule());
        let aad = [0u8; crate::wire::HEADER_LEN];

        let old = ring
            .cipher_for(0)
            .unwrap()
            .seal(50, 0, &aad, b"old")
            .unwrap();

        // The sender rotates; the straggler sealed under epoch 0 is still in flight.
        let new = ring
            .cipher_for(1)
            .unwrap()
            .seal(150, 0, &aad, b"new")
            .unwrap();
        assert_eq!(ring.current_epoch(), 1);

        let mut keys = MediaKeys::Ring(ring);
        assert_eq!(keys.open(1, 150, 0, &aad, &new).unwrap(), b"new");
        assert_eq!(
            keys.open(0, 50, 0, &aad, &old).unwrap(),
            b"old",
            "losing a frame purely because the key rotated underneath it is not acceptable on a \
             lossy link"
        );
    }

    #[test]
    fn the_ring_refuses_a_key_older_than_the_one_it_keeps() {
        let mut ring = KeyRing::new(schedule());
        let aad = [0u8; crate::wire::HEADER_LEN];

        // Advance three epochs; only the current and the one before it are held.
        for epoch in 1..=3u16 {
            ring.cipher_for(epoch);
        }
        assert_eq!(ring.current_epoch(), 3);
        assert!(ring.accepts(3));
        assert!(ring.accepts(2));
        assert!(
            !ring.accepts(1),
            "a key from three epochs ago must not be derivable on request"
        );

        // And the refusal is an authentication failure rather than a distinguishable "unknown
        // epoch": telling an attacker which of the two it was is a gift we would not use.
        let sealed = schedule()
            .cipher_for(1)
            .seal(10, 0, &aad, b"ancient")
            .unwrap();
        let mut keys = MediaKeys::Ring(ring);
        assert_eq!(
            keys.open(1, 10, 0, &aad, &sealed),
            Err(CryptoError::Authentication)
        );
    }

    #[test]
    fn learning_a_newer_epoch_advances_the_ring() {
        let mut ring = KeyRing::new(schedule());
        assert_eq!(ring.current_epoch(), 0);

        // A datagram from an epoch we have not seen yet: the sender has rotated and its datagrams
        // are arriving. The ring follows rather than rejecting.
        assert!(ring.cipher_for(1).is_some());
        assert_eq!(ring.current_epoch(), 1);

        // A jump, as a long loss burst would produce.
        assert!(ring.cipher_for(4).is_some());
        assert_eq!(ring.current_epoch(), 4);
        assert!(
            !ring.accepts(0),
            "the ring drifted forward and must not still hold the session's first key"
        );
    }

    #[test]
    fn the_schedule_does_not_print_its_secret() {
        let schedule = schedule();
        let rendered = format!("{schedule:?}");
        assert!(
            !rendered.contains("20"),
            "the secret must not be rendered: {rendered}"
        );
    }

    #[test]
    fn debug_does_not_leak_the_key() {
        let cipher = MediaCipher::new(&key(10));
        let rendered = format!("{cipher:?}");
        assert!(rendered.contains("redacted"));
        assert!(
            !rendered.contains("10"),
            "key bytes must not appear: {rendered}"
        );
    }
}
