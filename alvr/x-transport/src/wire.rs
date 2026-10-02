//! The datagram header: fixed-width, little-endian, no varints.
//!
//! Fixed width on purpose. This header is on the hot path of every datagram — 300 Mbps at
//! 1400-byte datagrams is ~27,000 parse/encode pairs per second — and a varint parser is a
//! branch per field for a saving of about ten bytes on a payload of fourteen hundred. The
//! whole header is 36 bytes, or 2.6 %, and its layout is one `copy_from_slice`.
//!
//! ```text
//!  0..8    frame_index          monotonic, allocated by the sender (ADR-0011's signal)
//!  8..16   target_timestamp_us  the frame's target display time
//! 16..20   frame_len            total payload bytes of the frame, for trimming the padding
//! 20..22   fragment_index       0 .. data_count + parity_count
//! 22..24   data_count
//! 24..26   parity_count
//! 26..28   fragment_len         payload bytes in *this* datagram
//! 28..32   send_seq             per-datagram counter, for the receiver's own loss accounting
//! 32..34   flags
//! 34..36   reserved             must be zero; rejected if not, so a future version cannot
//!                               be silently misread by this one
//! ```
//!
//! The header is also the AEAD's associated data ([`crate::crypto`]), so every field above
//! is authenticated even though it is not encrypted — a replayed or edited frame index
//! fails the tag rather than being believed.

use std::fmt;

/// The fixed header length, in bytes.
pub const HEADER_LEN: usize = 36;

/// Datagram flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Flags(pub u16);

impl Flags {
    pub const NONE: Flags = Flags(0);
    /// This datagram carries a parity shard rather than frame data.
    pub const PARITY: u16 = 1 << 0;
    /// This datagram is a retransmission of one already sent.
    pub const RETRANSMIT: u16 = 1 << 1;

    pub const fn is_parity(self) -> bool {
        self.0 & Self::PARITY != 0
    }

    pub const fn is_retransmit(self) -> bool {
        self.0 & Self::RETRANSMIT != 0
    }

    pub const fn with_parity(mut self) -> Self {
        self.0 |= Self::PARITY;
        self
    }

    pub const fn with_retransmit(mut self) -> Self {
        self.0 |= Self::RETRANSMIT;
        self
    }

    /// Reject anything this version does not define. An unknown flag means a peer that
    /// knows something we do not, and guessing is how a protocol version skew becomes a
    /// silent misread instead of an error.
    pub const fn is_valid(self) -> bool {
        self.0 & !(Self::PARITY | Self::RETRANSMIT) == 0
    }
}

/// A decoded datagram header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FragmentHeader {
    pub frame_index: u64,
    pub target_timestamp_us: u64,
    pub frame_len: u32,
    pub fragment_index: u16,
    pub data_count: u16,
    pub parity_count: u16,
    pub fragment_len: u16,
    pub send_seq: u32,
    pub flags: Flags,
}

/// Why a datagram could not be accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireError {
    /// Shorter than a header.
    TooShort,
    /// The declared payload length disagrees with the datagram's actual length.
    LengthMismatch { declared: u16, available: usize },
    /// The header describes something impossible (zero data fragments, a fragment index
    /// past the end of the frame, a non-zero reserved field, an unknown flag).
    Malformed(&'static str),
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WireError::TooShort => write!(f, "datagram shorter than a header"),
            WireError::LengthMismatch {
                declared,
                available,
            } => write!(
                f,
                "header declares {declared} payload bytes, datagram has {available}"
            ),
            WireError::Malformed(why) => write!(f, "malformed header: {why}"),
        }
    }
}

impl std::error::Error for WireError {}

impl FragmentHeader {
    /// Encode into a fresh header block.
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[0..8].copy_from_slice(&self.frame_index.to_le_bytes());
        out[8..16].copy_from_slice(&self.target_timestamp_us.to_le_bytes());
        out[16..20].copy_from_slice(&self.frame_len.to_le_bytes());
        out[20..22].copy_from_slice(&self.fragment_index.to_le_bytes());
        out[22..24].copy_from_slice(&self.data_count.to_le_bytes());
        out[24..26].copy_from_slice(&self.parity_count.to_le_bytes());
        out[26..28].copy_from_slice(&self.fragment_len.to_le_bytes());
        out[28..32].copy_from_slice(&self.send_seq.to_le_bytes());
        out[32..34].copy_from_slice(&self.flags.0.to_le_bytes());
        // 34..36 reserved, already zero
        out
    }

    /// Encode into a caller-supplied buffer, returning the datagram length.
    ///
    /// The receiver's counterpart of this is [`FragmentHeader::decode`]; both exist so the
    /// hot path never allocates a header on its own.
    pub fn write_into(&self, out: &mut [u8]) -> Result<usize, WireError> {
        let payload_end = HEADER_LEN
            .checked_add(self.fragment_len as usize)
            .ok_or(WireError::Malformed("fragment_len overflows"))?;
        if out.len() < payload_end {
            return Err(WireError::Malformed("output buffer too small"));
        }
        let header = self.encode();
        out[..HEADER_LEN].copy_from_slice(&header);
        Ok(payload_end)
    }

    /// Decode a datagram, validating it against its own length.
    pub fn decode(datagram: &[u8]) -> Result<(Self, &[u8]), WireError> {
        if datagram.len() < HEADER_LEN {
            return Err(WireError::TooShort);
        }

        let u16_at = |at: usize| u16::from_le_bytes([datagram[at], datagram[at + 1]]);
        let u32_at = |at: usize| {
            u32::from_le_bytes([
                datagram[at],
                datagram[at + 1],
                datagram[at + 2],
                datagram[at + 3],
            ])
        };
        let u64_at = |at: usize| {
            u64::from_le_bytes([
                datagram[at],
                datagram[at + 1],
                datagram[at + 2],
                datagram[at + 3],
                datagram[at + 4],
                datagram[at + 5],
                datagram[at + 6],
                datagram[at + 7],
            ])
        };

        let header = FragmentHeader {
            frame_index: u64_at(0),
            target_timestamp_us: u64_at(8),
            frame_len: u32_at(16),
            fragment_index: u16_at(20),
            data_count: u16_at(22),
            parity_count: u16_at(24),
            fragment_len: u16_at(26),
            send_seq: u32_at(28),
            flags: Flags(u16_at(32)),
        };

        if u16_at(34) != 0 {
            return Err(WireError::Malformed("reserved field is not zero"));
        }
        if !header.flags.is_valid() {
            return Err(WireError::Malformed("unknown flag bit"));
        }
        if header.data_count == 0 {
            return Err(WireError::Malformed("zero data fragments"));
        }
        let total = header.data_count as usize + header.parity_count as usize;
        if header.fragment_index as usize >= total {
            return Err(WireError::Malformed("fragment index past end of frame"));
        }
        // The parity flag and the index must agree; a datagram claiming to be parity while
        // sitting in the data range is either a bug or an attack, and either way the
        // receiver would put it in the wrong place.
        if header.flags.is_parity() != (header.fragment_index >= header.data_count) {
            return Err(WireError::Malformed(
                "parity flag disagrees with the fragment index",
            ));
        }

        let available = datagram.len() - HEADER_LEN;
        if available != header.fragment_len as usize {
            return Err(WireError::LengthMismatch {
                declared: header.fragment_len,
                available,
            });
        }
        if !header.flags.is_parity() && header.fragment_len == 0 {
            return Err(WireError::Malformed("empty data fragment"));
        }

        Ok((header, &datagram[HEADER_LEN..]))
    }

    pub const fn is_parity(&self) -> bool {
        self.flags.is_parity()
    }

    /// Datagram length for this header plus its payload.
    pub const fn datagram_len(&self) -> usize {
        HEADER_LEN + self.fragment_len as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> FragmentHeader {
        FragmentHeader {
            frame_index: 0x0102_0304_0506_0708,
            target_timestamp_us: 1_700_000_000_123_456,
            frame_len: 400_000,
            fragment_index: 7,
            data_count: 300,
            parity_count: 15,
            fragment_len: 1364,
            send_seq: 9_999,
            flags: Flags::NONE,
        }
    }

    #[test]
    fn round_trips() {
        let header = sample();
        let mut datagram = vec![0u8; header.datagram_len()];
        let len = header.write_into(&mut datagram).unwrap();
        assert_eq!(len, HEADER_LEN + 1364);

        let (decoded, payload) = FragmentHeader::decode(&datagram).unwrap();
        assert_eq!(decoded, header);
        assert_eq!(payload.len(), 1364);
    }

    #[test]
    fn round_trips_a_parity_fragment() {
        let mut header = sample();
        header.fragment_index = 305;
        header.flags = Flags::NONE.with_parity().with_retransmit();
        let datagram = header.encode();
        let mut full = datagram.to_vec();
        full.resize(HEADER_LEN + 1364, 0);
        let (decoded, _) = FragmentHeader::decode(&full).unwrap();
        assert!(decoded.is_parity());
        assert!(decoded.flags.is_retransmit());
    }

    #[test]
    fn rejects_a_short_datagram() {
        assert_eq!(
            FragmentHeader::decode(&[0u8; HEADER_LEN - 1]),
            Err(WireError::TooShort)
        );
    }

    #[test]
    fn rejects_a_length_disagreement() {
        // This is the check that stops a truncated datagram being decoded as a short
        // fragment, which would silently corrupt the frame instead of failing it.
        let header = sample();
        let mut datagram = header.encode().to_vec();
        datagram.resize(HEADER_LEN + 10, 0);
        assert!(matches!(
            FragmentHeader::decode(&datagram),
            Err(WireError::LengthMismatch { .. })
        ));
    }

    #[test]
    fn rejects_a_nonzero_reserved_field() {
        // Forward compatibility: a peer that speaks a later version of this header must
        // fail loudly here rather than be half-understood.
        let header = sample();
        let mut datagram = header.encode().to_vec();
        datagram.resize(HEADER_LEN + 1364, 0);
        datagram[34] = 1;
        assert!(matches!(
            FragmentHeader::decode(&datagram),
            Err(WireError::Malformed(_))
        ));
    }

    #[test]
    fn rejects_an_unknown_flag() {
        let header = sample();
        let mut datagram = header.encode().to_vec();
        datagram.resize(HEADER_LEN + 1364, 0);
        datagram[32] = 0b1000_0000;
        datagram[33] = 0;
        assert!(matches!(
            FragmentHeader::decode(&datagram),
            Err(WireError::Malformed(_))
        ));
    }

    #[test]
    fn rejects_a_fragment_index_past_the_end() {
        let mut header = sample();
        header.fragment_index = 315; // data_count + parity_count == 315 exactly: one past
        let mut datagram = header.encode().to_vec();
        datagram.resize(HEADER_LEN + 1364, 0);
        assert!(matches!(
            FragmentHeader::decode(&datagram),
            Err(WireError::Malformed(_))
        ));
    }

    #[test]
    fn rejects_a_parity_flag_that_disagrees_with_the_index() {
        let mut header = sample();
        header.flags = Flags::NONE.with_parity(); // but fragment_index 7 is data
        let mut datagram = header.encode().to_vec();
        datagram.resize(HEADER_LEN + 1364, 0);
        assert!(matches!(
            FragmentHeader::decode(&datagram),
            Err(WireError::Malformed(_))
        ));
    }

    #[test]
    fn rejects_zero_data_fragments() {
        let mut header = sample();
        header.data_count = 0;
        header.parity_count = 0;
        header.fragment_index = 0;
        let mut datagram = header.encode().to_vec();
        datagram.resize(HEADER_LEN + 1364, 0);
        assert!(matches!(
            FragmentHeader::decode(&datagram),
            Err(WireError::Malformed(_))
        ));
    }

    #[test]
    fn write_into_refuses_a_short_buffer() {
        let header = sample();
        let mut small = [0u8; 10];
        assert!(header.write_into(&mut small).is_err());
    }
}
