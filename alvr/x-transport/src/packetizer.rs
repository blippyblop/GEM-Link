//! Packetisation: turn one frame into datagrams, with repair shards.
//!
//! ## The shape of a frame on the wire
//!
//! ```text
//!   data 0 .. data_count-1   one datagram each, in order, carrying the frame's bytes
//!   parity 0 .. parity_count             repair shards over the same byte layout
//! ```
//!
//! Data shards are always full (`mtu - HEADER_LEN`) except the last, and every shard — data
//! and parity — is padded to that same length. Equal lengths are what make the FEC a
//! bytewise XOR; it costs at most one datagram's worth of padding per frame, and the
//! receiver trims using `frame_len` from the header rather than guessing.
//!
//! ## How much parity
//!
//! [`ParityPolicy`] is deliberately plain. A *ratio* is the useful default (5 % overhead
//! covers a single loss in the average frame and is the kind of number that shows up in
//! every real-time codec), and a *fixed* count is what you want when you know the link
//! loses one datagram per burst and not more. The policy only decides how many shards to
//! add; whether that is *enough* is the bench's question, not this module's.
//!
//! The repair is against **erasures**, which is the right model for a datagram plane: a
//! datagram that does not arrive is known to be missing (the header carries the count), so
//! the decoder never has to guess *which* bytes are wrong. That is a much easier problem
//! than error correction, and it means a modest amount of parity buys a lot.

use crate::{
    crypto::MediaCipher,
    fec,
    wire::{Flags, FragmentHeader, HEADER_LEN},
};

/// Per-frame identity: what the sender knows about a frame before it is cut up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameMeta {
    /// Monotonic, allocated at the earliest point the frame exists (ADR-0011's signal), and
    /// load-bearing for the AEAD nonce as well as for the loss check.
    pub frame_index: u64,
    /// The frame's target display time.
    pub target_timestamp_us: u64,
    /// Whether this frame is a keyframe. The client's trust gate cannot recover from a broken
    /// reference chain without it, so it travels on the wire (see [`crate::wire::Flags`]).
    pub is_keyframe: bool,
}

/// How many repair shards to add.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ParityPolicy {
    /// No FEC. Correct for a link that does not lose datagrams, and for measuring how much
    /// the FEC is worth.
    Off,
    /// A fraction of the data shards, rounded up.
    ///
    /// `ceil` rather than `floor` is deliberate and is the whole of the policy. A frame that
    /// is a **single** datagram gets one parity shard, i.e. 100 % overhead on that frame —
    /// and that is correct, not a rounding artefact: losing the only datagram of a frame
    /// loses the frame, and a lost frame breaks the decoder's reference chain, which is the
    /// mechanism the grey-frame defect ran on. `floor` would round every small frame's
    /// protection away, exactly where protection is cheapest and the consequence is worst.
    ///
    /// A fraction of `0.0` is the same as [`ParityPolicy::Off`].
    Ratio { fraction: f32 },
    /// Exactly this many, whatever the frame size.
    Fixed(u16),
}

impl ParityPolicy {
    /// Parity shards for **one FEC block** of `data_per_block` data shards.
    ///
    /// Per block rather than per frame, because a frame is split into blocks and each is
    /// coded independently ([`fec::blocks_for`]). A per-frame count would have to be divided
    /// up anyway, and dividing it here keeps the arithmetic in one place.
    pub fn per_block(&self, data_per_block: usize) -> usize {
        if data_per_block == 0 {
            return 0;
        }
        match *self {
            ParityPolicy::Off => 0,
            ParityPolicy::Fixed(n) => n as usize,
            ParityPolicy::Ratio { fraction } => {
                (data_per_block as f32 * fraction.max(0.0)).ceil() as usize
            }
        }
    }

    /// The full layout for a frame of `data_count` shards.
    pub fn plan(&self, data_count: usize) -> FramePlan {
        let data_count = data_count.max(1);
        let blocks = fec::blocks_for(data_count);
        let data_per_block = data_count.div_ceil(blocks);
        // Never more parity than the field can address alongside the block's data.
        let parity_per_block = self
            .per_block(data_per_block)
            .min(fec::MAX_SHARDS - data_per_block);
        FramePlan {
            blocks,
            data_count,
            data_per_block,
            parity_per_block,
            parity_count: parity_per_block * blocks,
        }
    }

    /// Total parity shards for a frame of `data_count` data shards.
    pub fn parity_for(&self, data_count: usize) -> usize {
        if data_count == 0 {
            return 0;
        }
        self.plan(data_count).parity_count
    }

    /// Overhead as a fraction of the frame, for reporting.
    pub fn overhead(&self, data_count: usize) -> f32 {
        if data_count == 0 {
            return 0.0;
        }
        self.parity_for(data_count) as f32 / data_count as f32
    }
}

/// How a frame of a given size is laid out: which FEC blocks exist and how many shards each
/// carries. A pure function of `(data_count, policy)`, so both ends derive it from the wire
/// header without any extra field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FramePlan {
    pub blocks: usize,
    pub data_count: usize,
    pub data_per_block: usize,
    pub parity_per_block: usize,
    pub parity_count: usize,
}

impl FramePlan {
    /// Which block a shard index (data or parity) belongs to.
    pub fn block_of(&self, index: usize) -> usize {
        fec::block_of(index, self.blocks)
    }

    /// Position of a shard inside its block.
    pub fn position_in_block(&self, index: usize) -> usize {
        fec::position_in_block(index, self.blocks)
    }

    /// The wire index of a parity shard: block `b`, position `p`.
    pub fn parity_index(&self, block: usize, position: usize) -> usize {
        position * self.blocks + block
    }
}

/// Which datagrams the packetiser produced, for the sender's own bookkeeping (retransmit
/// needs to know what a fragment index means).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameLayout {
    pub frame_index: u64,
    pub frame_len: u32,
    pub data_count: u16,
    pub parity_count: u16,
    /// FEC blocks the frame was split into. Derived from `data_count` by both ends, carried
    /// here only so the sender's bookkeeping and the tests can read it.
    pub blocks: u16,
    pub parity_per_block: u16,
    /// Datagram length for a data shard, i.e. `mtu`. Parity shards are the same length.
    pub datagram_len: usize,
}

impl FrameLayout {
    pub const fn total_fragments(&self) -> u16 {
        self.data_count + self.parity_count
    }

    /// Whether a fragment index is a repair shard.
    pub const fn is_parity(&self, fragment_index: u16) -> bool {
        fragment_index >= self.data_count
    }
}

/// Cuts frames into datagrams.
#[derive(Debug, Clone)]
pub struct Packetizer {
    mtu: usize,
    parity: ParityPolicy,
}

impl Packetizer {
    /// `mtu` is the whole datagram budget including the header.
    pub fn new(mtu: usize, parity: ParityPolicy) -> Self {
        assert!(
            mtu > HEADER_LEN,
            "an MTU of {mtu} leaves no room for a payload"
        );
        Self { mtu, parity }
    }

    pub const fn mtu(&self) -> usize {
        self.mtu
    }

    pub const fn parity_policy(&self) -> ParityPolicy {
        self.parity
    }

    /// Payload bytes per datagram.
    pub const fn shard_len(&self) -> usize {
        self.mtu - HEADER_LEN
    }

    /// Fragment a frame into wire datagrams, in send order.
    ///
    /// `send_seq` is bumped once per datagram and stamped into every header; the receiver
    /// uses it for its own loss accounting, which is the only way either end can tell how
    /// much of a frame never arrived.
    ///
    /// With a [`MediaCipher`], every datagram is sealed with the header as associated data —
    /// including the parity shards, which are sealed as their own datagrams rather than left
    /// in the clear.
    pub fn fragment(
        &self,
        meta: FrameMeta,
        payload: &[u8],
        send_seq: &mut u32,
        cipher: Option<&MediaCipher>,
    ) -> Result<(FrameLayout, Vec<Vec<u8>>), PacketizeError> {
        let frame_len = payload.len();
        if frame_len > u32::MAX as usize {
            return Err(PacketizeError::FrameTooLarge(frame_len));
        }
        let shard_len = self.shard_len();

        let data_count = frame_len.div_ceil(shard_len).max(1);
        let plan = self.parity.plan(data_count);
        let total = data_count + plan.parity_count;
        if total > u16::MAX as usize {
            return Err(PacketizeError::TooManyFragments(total));
        }

        // Data shards, padded to a common length so the FEC is a bytewise XOR.
        let mut shards: Vec<Vec<u8>> = Vec::with_capacity(total);
        for i in 0..data_count {
            let start = i * shard_len;
            let end = (start + shard_len).min(frame_len);
            let mut shard = vec![0u8; shard_len];
            shard[..end - start].copy_from_slice(&payload[start..end]);
            shards.push(shard);
        }

        // Parity, one block at a time. The data shards are already in wire order, so a block
        // is every `blocks`-th shard — which is also why a burst loss is spread across the
        // blocks instead of landing in one of them.
        if plan.parity_per_block > 0 {
            let mut parity: Vec<Vec<u8>> = vec![Vec::new(); plan.parity_count];
            for block in 0..plan.blocks {
                let block_data: Vec<Vec<u8>> = (0..data_count)
                    .filter(|i| plan.block_of(*i) == block)
                    .map(|i| shards[i].clone())
                    .collect();
                let block_parity =
                    fec::encode(&block_data, plan.parity_per_block).map_err(PacketizeError::Fec)?;
                for (position, shard) in block_parity.into_iter().enumerate() {
                    parity[plan.parity_index(block, position)] = shard;
                }
            }
            shards.extend(parity);
        }

        let layout = FrameLayout {
            frame_index: meta.frame_index,
            frame_len: frame_len as u32,
            data_count: data_count as u16,
            parity_count: plan.parity_count as u16,
            blocks: plan.blocks as u16,
            parity_per_block: plan.parity_per_block as u16,
            datagram_len: self.mtu,
        };

        let wire_payload_len = if cipher.is_some() {
            shard_len + crate::crypto::TAG_LEN
        } else {
            shard_len
        };

        let mut datagrams = Vec::with_capacity(total);
        for (fragment_index, shard) in shards.iter().enumerate() {
            let is_parity = fragment_index >= data_count;
            let mut flags = if is_parity {
                Flags::NONE.with_parity()
            } else {
                Flags::NONE
            };
            // On every shard, parity included: an FEC-repaired frame is completed by whichever
            // shard arrives last, and that is frequently a parity shard.
            if meta.is_keyframe {
                flags = flags.with_keyframe();
            }

            let header = FragmentHeader {
                frame_index: meta.frame_index,
                target_timestamp_us: meta.target_timestamp_us,
                frame_len: frame_len as u32,
                fragment_index: fragment_index as u16,
                data_count: data_count as u16,
                parity_count: plan.parity_count as u16,
                fragment_len: wire_payload_len as u16,
                send_seq: *send_seq,
                flags,
            };
            *send_seq = send_seq.wrapping_add(1);

            let mut datagram = vec![0u8; HEADER_LEN + wire_payload_len];
            let header_bytes = header.encode();
            datagram[..HEADER_LEN].copy_from_slice(&header_bytes);

            match cipher {
                Some(cipher) => {
                    cipher
                        .seal_into(
                            meta.frame_index,
                            fragment_index as u16,
                            &header_bytes,
                            shard,
                            &mut datagram[HEADER_LEN..],
                        )
                        .map_err(PacketizeError::Crypto)?;
                }
                None => datagram[HEADER_LEN..].copy_from_slice(shard),
            }

            datagrams.push(datagram);
        }

        Ok((layout, datagrams))
    }
}

/// Why a frame could not be packetised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketizeError {
    /// Larger than a frame index can describe.
    FrameTooLarge(usize),
    /// More shards than the wire format or the field can address.
    TooManyFragments(usize),
    /// The FEC rejected the shape.
    Fec(fec::FecError),
    /// Sealing failed.
    Crypto(crate::crypto::CryptoError),
}

impl std::fmt::Display for PacketizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PacketizeError::FrameTooLarge(n) => write!(f, "frame of {n} bytes is too large"),
            PacketizeError::TooManyFragments(n) => write!(f, "{n} fragments is too many"),
            PacketizeError::Fec(e) => write!(f, "FEC: {e}"),
            PacketizeError::Crypto(e) => write!(f, "crypto: {e}"),
        }
    }
}

impl std::error::Error for PacketizeError {}

#[cfg(test)]
mod tests {
    use super::*;

    const MTU: usize = 1400;
    const SHARD: usize = MTU - HEADER_LEN;

    fn payload(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    fn meta() -> FrameMeta {
        FrameMeta {
            frame_index: 1,
            target_timestamp_us: 11_111,
            is_keyframe: false,
        }
    }

    #[test]
    fn a_small_frame_is_one_data_shard() {
        let p = Packetizer::new(MTU, ParityPolicy::Off);
        let mut seq = 0;
        let (layout, datagrams) = p.fragment(meta(), &payload(100), &mut seq, None).unwrap();
        assert_eq!(layout.data_count, 1);
        assert_eq!(layout.parity_count, 0);
        assert_eq!(datagrams.len(), 1);
        // One short frame still gets one repair shard at a ratio policy: losing the only
        // datagram of a frame loses the frame, and a lost frame breaks the reference chain,
        // so the 100 % overhead on a small frame is the correct trade.
        let p = Packetizer::new(MTU, ParityPolicy::Ratio { fraction: 0.1 });
        let (layout, _) = p.fragment(meta(), &payload(100), &mut 0, None).unwrap();
        assert_eq!(layout.parity_count, 1);
    }

    #[test]
    fn exact_multiples_do_not_gain_a_spare_shard() {
        let p = Packetizer::new(MTU, ParityPolicy::Off);
        let (layout, datagrams) = p
            .fragment(meta(), &payload(SHARD * 3), &mut 0, None)
            .unwrap();
        assert_eq!(layout.data_count, 3);
        assert_eq!(datagrams.len(), 3);
    }

    #[test]
    fn a_split_frame_carries_its_length() {
        let p = Packetizer::new(MTU, ParityPolicy::Off);
        let bytes = payload(SHARD * 2 + 17);
        let (layout, datagrams) = p.fragment(meta(), &bytes, &mut 0, None).unwrap();
        assert_eq!(layout.data_count, 3);
        assert_eq!(layout.frame_len as usize, bytes.len());

        // The last data shard is padded, and the header says how much to keep.
        let (header, body) = FragmentHeader::decode(&datagrams[2]).unwrap();
        assert_eq!(header.frame_len as usize, bytes.len());
        assert_eq!(body.len(), SHARD, "shards are equal length for the FEC");
        assert_eq!(&body[..17], &bytes[SHARD * 2..]);
        assert!(body[17..].iter().all(|b| *b == 0), "padding must be zero");
    }

    #[test]
    fn fragment_indices_are_sequential_and_flagged_consistently() {
        let p = Packetizer::new(MTU, ParityPolicy::Fixed(3));
        let (layout, datagrams) = p
            .fragment(meta(), &payload(SHARD * 4), &mut 0, None)
            .unwrap();
        assert_eq!(layout.data_count, 4);
        assert_eq!(layout.parity_count, 3);

        for (i, datagram) in datagrams.iter().enumerate() {
            let (header, _) = FragmentHeader::decode(datagram).unwrap();
            assert_eq!(header.fragment_index as usize, i);
            assert_eq!(header.data_count, 4);
            assert_eq!(header.parity_count, 3);
            assert_eq!(header.is_parity(), i >= 4);
        }
    }

    #[test]
    fn send_seq_increments_once_per_datagram() {
        let p = Packetizer::new(MTU, ParityPolicy::Fixed(2));
        let mut seq = 100;
        let (_, first) = p
            .fragment(meta(), &payload(SHARD * 3), &mut seq, None)
            .unwrap();
        assert_eq!(seq, 100 + first.len() as u32);
        let (_, second) = p.fragment(meta(), &payload(SHARD), &mut seq, None).unwrap();
        assert_eq!(seq, 100 + first.len() as u32 + second.len() as u32);
        let (header, _) = FragmentHeader::decode(&second[0]).unwrap();
        assert_eq!(header.send_seq, 100 + first.len() as u32);
    }

    #[test]
    fn an_empty_frame_still_produces_one_datagram() {
        // A zero-length frame is legal (an all-black frame compresses to nothing at the
        // codec layer) and must not produce zero datagrams, or the receiver would never
        // see the frame index advance.
        let p = Packetizer::new(MTU, ParityPolicy::Off);
        let (layout, datagrams) = p.fragment(meta(), &[], &mut 0, None).unwrap();
        assert_eq!(layout.data_count, 1);
        assert_eq!(datagrams.len(), 1);
    }

    #[test]
    fn a_frame_too_big_for_the_wire_format_is_refused() {
        // The fragment index is 16 bits, so 65535 datagrams is the hard ceiling. Striping
        // lifted the *field* limit, so this is what is left — and it is genuinely far away:
        // at a 1400-byte MTU it is an 89 MB frame, which no codec will produce. The test uses
        // a small MTU so it does not have to allocate one to prove the check exists.
        let small_mtu = 100; // 64-byte shards
        let p = Packetizer::new(small_mtu, ParityPolicy::Off);
        let count = 70_000 * (small_mtu - HEADER_LEN);
        assert!(matches!(
            p.fragment(meta(), &payload(count), &mut 0, None),
            Err(PacketizeError::TooManyFragments(_))
        ));
    }

    #[test]
    fn the_ratio_policy_scales_and_caps() {
        let policy = ParityPolicy::Ratio { fraction: 0.05 };
        // Ceil, not floor: a one-shard frame gets a shard, for the reason in the docs.
        assert_eq!(policy.parity_for(1), 1);
        assert_eq!(policy.parity_for(2), 1);
        assert_eq!(policy.parity_for(100), 5);
        // 300 shards is two blocks of 150, each with ceil(150 * 0.05) = 8 parity shards.
        // The count is per block, so the frame total is 16, not ceil(300 * 0.05) = 15 — one
        // extra shard buys whole-block integrity rather than a fraction of one.
        assert_eq!(policy.parity_for(300), 16);
        let plan = policy.plan(300);
        assert_eq!(plan.blocks, 2);
        assert_eq!(plan.data_per_block, 150);
        assert_eq!(plan.parity_per_block, 8);
        // Never more than the field allows alongside the block's data.
        assert!(plan.data_per_block + plan.parity_per_block <= fec::MAX_SHARDS);
        assert_eq!(policy.parity_for(0), 0);
        // An empty frame has nothing to protect.
        assert_eq!(policy.parity_for(0), 0);
        // A zero ratio is a no-op, so a settings value of 0 means "off".
        assert_eq!(ParityPolicy::Ratio { fraction: 0.0 }.parity_for(100), 0);
    }

    #[test]
    fn a_frame_of_any_realistic_size_is_plannable() {
        // The reason striping exists: at the Frame envelope one frame is ~300 shards, and a
        // single GFP(2^8) block could not address it.
        for shards in [1usize, 100, 250, 251, 300, 500, 1000] {
            let plan = ParityPolicy::Ratio { fraction: 0.05 }.plan(shards);
            assert_eq!(plan.data_count, shards);
            assert!(
                plan.data_per_block + plan.parity_per_block <= fec::MAX_SHARDS,
                "{shards} shards: a block needs {} + {} elements",
                plan.data_per_block,
                plan.parity_per_block
            );
            assert!(plan.parity_count > 0, "{shards} shards got no protection");
        }
    }

    #[test]
    fn sealed_datagrams_carry_no_plaintext() {
        let key = [7u8; crate::crypto::KEY_LEN];
        let cipher = MediaCipher::new(&key);
        let p = Packetizer::new(MTU, ParityPolicy::Fixed(1));
        let bytes = payload(SHARD + 10);
        let (_, datagrams) = p.fragment(meta(), &bytes, &mut 0, Some(&cipher)).unwrap();

        // The seal adds a tag, and the payload is not recognisable on the wire.
        let (header, body) = FragmentHeader::decode(&datagrams[0]).unwrap();
        assert_eq!(header.fragment_len as usize, SHARD + crate::crypto::TAG_LEN);
        assert_ne!(body, &bytes[..SHARD]);

        // And it opens back to the padded shard.
        let opened = cipher
            .open(
                header.frame_index,
                header.fragment_index,
                &header.encode(),
                body,
            )
            .unwrap();
        assert_eq!(opened, &bytes[..SHARD]);
    }
}
