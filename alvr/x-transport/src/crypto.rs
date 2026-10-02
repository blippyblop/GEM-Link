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
