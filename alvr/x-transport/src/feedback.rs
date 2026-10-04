//! What the client tells the sender, and how it is framed.
//!
//! The receive path has been able to say three things since it was written — *these fragments of
//! this frame never arrived*, *nothing has progressed, send a keyframe*, and *I have given up and am
//! rebuilding from here* — and there was **nothing that carried any of them back**. A repair
//! mechanism the sender cannot hear is not a mechanism; the receiver could name the exact fragments
//! it was missing and then wait for a keyframe instead.
//!
//! ## Where these travel, and why not on the control socket
//!
//! A NACK is the most latency-critical message either end sends: by the time it arrives, the frame
//! it names has a deadline measured in single-digit milliseconds. The control socket is TCP, framed,
//! and carries logs — so a NACK would queue behind whatever else is on it, and one lost TCP segment
//! would hold every NACK behind it. So feedback rides back on the **media socket**, as its own
//! datagram.
//!
//! That means it has to be authenticated, or anyone who can reach the port can demand retransmits
//! (an amplifier: a 12-byte request for 1.4 KB) or a stream reset. It is sealed with a key
//! **domain-separated from the media keys** — see [`crate::crypto::KeySchedule::feedback_key`] — and
//! the nonce comes from a counter only the client owns, so no feedback message can ever share a
//! nonce with a media frame.
//!
//! ## The framing is hand-rolled
//!
//! `bincode` and `serde` are not on this crate's hot path and this is not a place to introduce them:
//! the messages are three fixed layouts, and a codec that is twenty lines is one that can be read in
//! full. Every length is bounded before it is trusted, because the input arrives from the network.

use std::time::Duration;

use crate::crypto::{CryptoError, MediaCipher};

/// The most fragments one `Nack` may name.
///
/// Bounded so a hostile or corrupt datagram cannot ask the receiver to allocate: at two bytes each
/// this is 512 bytes of payload, comfortably inside any MTU, and a frame that is missing more than
/// this many fragments is not worth repairing — the sender will send a keyframe instead, because
/// `MediaSender` refuses a repair that cannot arrive in time.
pub const MAX_NACK_FRAGMENTS: usize = 256;

/// The largest a feedback datagram can be, for sizing a receive buffer.
pub const MAX_FEEDBACK_LEN: usize = 1 + 8 + 2 + MAX_NACK_FRAGMENTS * 2 + crate::crypto::TAG_LEN;

const TAG_NACK: u8 = 1;
const TAG_REQUEST_KEYFRAME: u8 = 2;
const TAG_STREAM_RESET: u8 = 3;
/// The client's own queueing delay, in microseconds.
///
/// The one thing the client can say that tells the sender it is **outrunning** the receiver. Every
/// other signal is ambiguous from the client's side — a shard that never arrived and a shard still
/// queued behind it are the same fact locally — and that ambiguity is what made three attempts at
/// fixing the repair path worse than leaving it alone. This is not a fact about a frame; it is a
/// fact about the receiver, and it is what the sender needs in order to send less.
const TAG_QUEUE_DELAY: u8 = 4;
/// **This frame was decoded.** The acknowledgement the sender's encoder needs, and the one signal
/// that makes a lost frame stop poisoning the stream.
///
/// Every other message the client sends is a complaint; this is the only one that says a frame
/// *worked*. The server cannot infer it: a frame it put on the wire may have been dropped by the
/// kernel, lost in the queue, refused by the FEC, or never decoded — and from the sender's side all
/// of those look identical to success until something else breaks.
///
/// What the sender does with it, in order of importance:
///
/// 1. **It tells the encoder what it may reference.** A frame is encoded against a frame the client
///    has acknowledged, so the reference chain never includes a frame the client does not have, and
///    a lost frame no longer corrupts everything behind it. The cost is that references are a little
///    older, which is a compression cost and not a correctness one.
/// 2. **It is the honest loss number.** Frames the client never acknowledges are frames it could not
///    use, and that is a measurement, not an inference from repair requests — which is what the
///    parity controller had to work with before, and why it read the client's *queueing* as loss.
/// 3. It stops a repair being sent for a frame that has already been delivered.
///
/// Sent per decoded frame rather than as a cursor, because a client that skipped a frame has not
/// received the ones before it in any useful sense — the indices that matter are the ones that
/// decoded.
const TAG_ACK: u8 = 5;

/// One message from the client to the sender.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Feedback {
    /// These fragments of this frame never arrived. The repair that costs one round trip and saves
    /// the frame, the reference chain and the next 30 frames behind it.
    Nack {
        frame_index: u64,
        fragments: Vec<u16>,
    },
    /// Nothing has progressed for the ask threshold. The encoder inserts a keyframe only when asked,
    /// so this is the only way out of a hold.
    RequestKeyframe {
        /// The newest frame the client has seen, so the sender can tell whether the keyframe it is
        /// about to send is already stale.
        newest_frame: u64,
    },
    /// The client is rebuilding. `last_presented` is the newest frame it actually showed, which is
    /// where the stream must resume from — **not** from a keyframe it has not received, and not from
    /// the last frame the sender *sent*, which is a different number and the whole reason this field
    /// exists (the reference client logs `SVLFEC::ReportStreamReset() (m_iLastAcknowledgeFrameNumber
    /// = %d)` for exactly this).
    StreamReset { last_presented: u64 },
    /// The client's own queueing delay — how long it is taking to read frames, as an EWMA in
    /// microseconds. See [`TAG_QUEUE_DELAY`].
    ///
    /// Sent on its own slow cadence, not per frame: it is a property of the receiver measured over
    /// many frames, and a control loop fed per-frame noise is a control loop that oscillates.
    QueueDelay {
        micros: u32,
        /// What share of each frame's **declared** shards never arrived, in tenths of a percent.
        ///
        /// The measurement the queueing delay cannot make: a drain spread is only observable on
        /// frames that completed, and a client losing half of every frame completes only the small
        /// ones. Measured on the live rig, that client reported 21 ms of "queueing delay" — one and a
        /// half frames behind, the mildest rung of the ladder — while receiving a third of every
        /// frame. This is what the sender has to send less *of*.
        missing_per_mille: u16,
        /// What share of the fragments the client asked for, since the last report, **arrived late
        /// rather than never** — in tenths of a percent, so it fits in a `u16` with room to spare.
        ///
        /// These are requests that were answered too late to help, and they are **not loss**. A share
        /// rather than a count because the client's reporting interval and the sender's loss window
        /// are different lengths and both are ours: a count has to be credited to the interval it was
        /// measured over, and two earlier attempts to do that got it wrong — one subtracted once per
        /// report against a window holding thousands of requests, the other dumped a 30-frame count
        /// into an 8-frame window that then reset and took most of it away. A share needs no
        /// interval: the sender applies it to its own window's requests.
        late_per_mille: u16,
    },
    /// This frame was decoded and is in the decoder's reference chain — see [`TAG_ACK`].
    Ack { frame_index: u64 },
}

/// Why a feedback datagram could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackError {
    /// Shorter than the tag, or shorter than the fields the tag implies.
    Truncated,
    /// A tag this version does not define. Rejected rather than guessed at, like an unknown flag bit.
    UnknownTag(u8),
    /// A length that would require more bytes than the datagram has, or more than the cap.
    Length,
}

impl std::fmt::Display for FeedbackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FeedbackError::Truncated => f.write_str("feedback datagram is truncated"),
            FeedbackError::UnknownTag(tag) => write!(f, "unknown feedback tag {tag}"),
            FeedbackError::Length => f.write_str("feedback datagram declares an impossible length"),
        }
    }
}

impl std::error::Error for FeedbackError {}

impl Feedback {
    /// Encode to a datagram body. Never fails: every variant is bounded by construction.
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Feedback::Nack {
                frame_index,
                fragments,
            } => {
                let fragments = &fragments[..fragments.len().min(MAX_NACK_FRAGMENTS)];
                let mut out = Vec::with_capacity(11 + fragments.len() * 2);
                out.push(TAG_NACK);
                out.extend_from_slice(&frame_index.to_le_bytes());
                out.extend_from_slice(&(fragments.len() as u16).to_le_bytes());
                for fragment in fragments {
                    out.extend_from_slice(&fragment.to_le_bytes());
                }
                out
            }
            Feedback::RequestKeyframe { newest_frame } => {
                let mut out = Vec::with_capacity(9);
                out.push(TAG_REQUEST_KEYFRAME);
                out.extend_from_slice(&newest_frame.to_le_bytes());
                out
            }
            Feedback::StreamReset { last_presented } => {
                let mut out = Vec::with_capacity(9);
                out.push(TAG_STREAM_RESET);
                out.extend_from_slice(&last_presented.to_le_bytes());
                out
            }
            Feedback::QueueDelay {
                micros,
                late_per_mille,
                missing_per_mille,
            } => {
                let mut out = Vec::with_capacity(13);
                out.push(TAG_QUEUE_DELAY);
                out.extend_from_slice(&micros.to_le_bytes());
                out.extend_from_slice(&late_per_mille.to_le_bytes());
                out.extend_from_slice(&missing_per_mille.to_le_bytes());
                out
            }
            Feedback::Ack { frame_index } => {
                let mut out = Vec::with_capacity(9);
                out.push(TAG_ACK);
                out.extend_from_slice(&frame_index.to_le_bytes());
                out
            }
        }
    }

    /// Read a datagram body.
    pub fn decode(bytes: &[u8]) -> Result<Self, FeedbackError> {
        let (&tag, body) = bytes.split_first().ok_or(FeedbackError::Truncated)?;

        let u64_at = |at: usize| -> Result<u64, FeedbackError> {
            let slice = body.get(at..at + 8).ok_or(FeedbackError::Truncated)?;
            Ok(u64::from_le_bytes(slice.try_into().expect("8 bytes")))
        };

        match tag {
            TAG_NACK => {
                let frame_index = u64_at(0)?;
                let count = u16::from_le_bytes(
                    body.get(8..10)
                        .ok_or(FeedbackError::Truncated)?
                        .try_into()
                        .expect("2 bytes"),
                ) as usize;

                if count > MAX_NACK_FRAGMENTS {
                    // Refused before allocating: the cap is the whole reason this check is first.
                    return Err(FeedbackError::Length);
                }
                let needed = 10 + count * 2;
                if body.len() < needed {
                    return Err(FeedbackError::Length);
                }

                let fragments = body[10..needed]
                    .chunks_exact(2)
                    .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                    .collect();

                Ok(Feedback::Nack {
                    frame_index,
                    fragments,
                })
            }
            TAG_REQUEST_KEYFRAME => Ok(Feedback::RequestKeyframe {
                newest_frame: u64_at(0)?,
            }),
            TAG_STREAM_RESET => Ok(Feedback::StreamReset {
                last_presented: u64_at(0)?,
            }),
            TAG_QUEUE_DELAY => {
                let micros = u32::from_le_bytes(
                    body.get(0..4)
                        .ok_or(FeedbackError::Truncated)?
                        .try_into()
                        .expect("4 bytes"),
                );
                let late_per_mille = u16::from_le_bytes(
                    body.get(4..6)
                        .ok_or(FeedbackError::Truncated)?
                        .try_into()
                        .expect("2 bytes"),
                );
                let missing_per_mille = u16::from_le_bytes(
                    body.get(6..8)
                        .ok_or(FeedbackError::Truncated)?
                        .try_into()
                        .expect("2 bytes"),
                );
                Ok(Feedback::QueueDelay {
                    micros,
                    late_per_mille,
                    missing_per_mille,
                })
            }
            TAG_ACK => Ok(Feedback::Ack {
                frame_index: u64_at(0)?,
            }),
            other => Err(FeedbackError::UnknownTag(other)),
        }
    }

    /// The frame this message is about, for logging and for the sender's deadline check.
    pub fn frame_index(&self) -> u64 {
        match self {
            Feedback::Nack { frame_index, .. } => *frame_index,
            Feedback::RequestKeyframe { newest_frame } => *newest_frame,
            Feedback::StreamReset { last_presented } => *last_presented,
            // Not about a frame: it is about the receiver. A caller needing a frame for a deadline
            // check must not get one from here, and `u64::MAX` makes the deadline rule refuse rather
            // than repair something arbitrary.
            Feedback::QueueDelay { .. } => u64::MAX,
            Feedback::Ack { frame_index } => *frame_index,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Feedback::Nack { .. } => "nack",
            Feedback::RequestKeyframe { .. } => "request-keyframe",
            Feedback::StreamReset { .. } => "stream-reset",
            Feedback::QueueDelay { .. } => "queue-delay",
            Feedback::Ack { .. } => "ack",
        }
    }

    /// Split a NACK that names more fragments than one datagram can carry.
    ///
    /// A frame at 300 Mbps has more than `MAX_NACK_FRAGMENTS` shards, so a badly damaged one names
    /// more fragments than fit — and truncating instead of splitting would silently ask for the
    /// first 256 and let the frame die for want of the rest, which is a repair that looks like it
    /// happened.
    pub fn nack_chunks(frame_index: u64, fragments: &[u16]) -> Vec<Feedback> {
        if fragments.is_empty() {
            return Vec::new();
        }
        fragments
            .chunks(MAX_NACK_FRAGMENTS)
            .map(|chunk| Feedback::Nack {
                frame_index,
                fragments: chunk.to_vec(),
            })
            .collect()
    }
}

/// The client's side of the feedback channel: seal a message with a sequence number only this end
/// uses.
///
/// The sequence is the nonce's frame component, and it is monotonic for the life of the key — the
/// same invariant the media plane relies on, applied to a key that no media datagram is sealed
/// under. A feedback message therefore cannot collide with a frame, whatever the frame indices do.
#[derive(Debug)]
pub struct FeedbackSender {
    cipher: MediaCipher,
    sequence: u64,
}

impl FeedbackSender {
    pub fn new(cipher: MediaCipher) -> Self {
        Self {
            cipher,
            sequence: 0,
        }
    }

    pub fn messages_sent(&self) -> u64 {
        self.sequence
    }

    /// Seal one message into `out`, returning the datagram length.
    pub fn seal(&mut self, feedback: &Feedback, out: &mut [u8]) -> Result<usize, CryptoError> {
        let body = feedback.encode();
        let sequence = self.sequence;
        // Fragment index 0: a feedback datagram is never split, so the second nonce component is a
        // constant and all the entropy is in the sequence.
        let written = self.cipher.seal_into(sequence, 0, &[], &body, out)?;
        self.sequence += 1;
        Ok(written)
    }
}

/// The sender's side: open a feedback datagram and count what it says.
#[derive(Debug)]
pub struct FeedbackReceiver {
    cipher: MediaCipher,
    /// The highest sequence seen. A datagram at or below it is a replay or a duplicate, and is
    /// counted rather than acted on — acting on a replayed NACK is a retransmit storm an attacker
    /// can drive from a single captured packet.
    highest_sequence: Option<u64>,
    replays: u64,
    unauthenticated: u64,
}

impl FeedbackReceiver {
    pub fn new(cipher: MediaCipher) -> Self {
        Self {
            cipher,
            highest_sequence: None,
            replays: 0,
            unauthenticated: 0,
        }
    }

    pub fn replays(&self) -> u64 {
        self.replays
    }

    pub fn unauthenticated(&self) -> u64 {
        self.unauthenticated
    }

    /// Open one datagram.
    ///
    /// Returns `Ok(None)` for an authentic message that is a replay, which is a *decision* rather
    /// than an error: the caller does nothing, and the count says so.
    pub fn open(&mut self, datagram: &[u8]) -> Result<Option<Feedback>, FeedbackOpenError> {
        // The sequence is not on the wire: it is tried from the expected value. Feedback is
        // lossy and may arrive out of order, so a small window is searched rather than one
        // value.
        const WINDOW: u64 = 64;

        let expected = self.highest_sequence.map_or(0, |highest| highest + 1);

        for candidate in expected.saturating_sub(WINDOW)..expected + WINDOW {
            let Ok(body) = self.cipher.open(candidate, 0, &[], datagram) else {
                continue;
            };
            let feedback = Feedback::decode(&body).map_err(FeedbackOpenError::Malformed)?;

            if self
                .highest_sequence
                .is_some_and(|highest| candidate <= highest)
            {
                self.replays += 1;
                return Ok(None);
            }
            self.highest_sequence = Some(candidate);
            return Ok(Some(feedback));
        }

        self.unauthenticated += 1;
        Err(FeedbackOpenError::Unauthenticated)
    }
}

/// Why a feedback datagram could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackOpenError {
    /// The tag verified under no sequence in the window.
    Unauthenticated,
    /// Authentic, but the body is not a message this version understands.
    Malformed(FeedbackError),
}

impl std::fmt::Display for FeedbackOpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FeedbackOpenError::Unauthenticated => {
                f.write_str("feedback datagram failed authentication")
            }
            FeedbackOpenError::Malformed(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for FeedbackOpenError {}

/// How long a client waits between NACKs for the same frame.
///
/// One round trip plus slack: asking twice inside a round trip cannot be answered twice, and each
/// ask costs the sender a datagram's worth of work. The caller owns the clock, so this is a
/// constant rather than a timer.
pub fn nack_retry_interval(rtt: Duration) -> Duration {
    // One round trip, floored at a frame interval's worth of slack so a very fast link does not
    // produce a NACK per millisecond.
    let floor = Duration::from_millis(4);
    (rtt * 2).max(floor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{KEY_LEN, KeySchedule};

    #[test]
    fn every_message_round_trips() {
        let messages = [
            Feedback::Nack {
                frame_index: 42,
                fragments: vec![0, 5, 300],
            },
            Feedback::RequestKeyframe { newest_frame: 9 },
            Feedback::StreamReset {
                last_presented: 1_000_000,
            },
            Feedback::QueueDelay {
                micros: 41_000,
                late_per_mille: 123,
                missing_per_mille: 7,
            },
        ];

        for message in messages {
            let encoded = message.encode();
            assert_eq!(Feedback::decode(&encoded), Ok(message));
        }
    }

    #[test]
    fn a_nack_that_names_more_fragments_than_one_datagram_holds_is_split_not_truncated() {
        let fragments: Vec<u16> = (0..600).collect();
        let chunks = Feedback::nack_chunks(7, &fragments);

        assert_eq!(chunks.len(), 3);
        let mut round_tripped = Vec::new();
        for chunk in &chunks {
            match Feedback::decode(&chunk.encode()).unwrap() {
                Feedback::Nack { fragments, .. } => round_tripped.extend(fragments),
                other => panic!("expected a nack, got {other:?}"),
            }
        }
        assert_eq!(
            round_tripped, fragments,
            "splitting lost fragments, so the frame would die for want of the ones that were dropped"
        );
    }

    #[test]
    fn an_empty_nack_is_not_sent_at_all() {
        assert!(Feedback::nack_chunks(1, &[]).is_empty());
    }

    #[test]
    fn a_truncated_or_lying_datagram_is_refused_rather_than_allocated_for() {
        assert_eq!(Feedback::decode(&[]), Err(FeedbackError::Truncated));
        assert_eq!(Feedback::decode(&[99]), Err(FeedbackError::UnknownTag(99)));

        // A NACK claiming 60,000 fragments in an eleven-byte datagram. The count is checked before
        // anything is sized against it.
        let mut lying = vec![TAG_NACK];
        lying.extend_from_slice(&1u64.to_le_bytes());
        lying.extend_from_slice(&60_000u16.to_le_bytes());
        assert_eq!(Feedback::decode(&lying), Err(FeedbackError::Length));

        // And one that is merely short of the count it declares.
        let mut short = vec![TAG_NACK];
        short.extend_from_slice(&1u64.to_le_bytes());
        short.extend_from_slice(&4u16.to_le_bytes());
        short.extend_from_slice(&[0, 1]);
        assert_eq!(Feedback::decode(&short), Err(FeedbackError::Length));
    }

    fn ciphers() -> (FeedbackSender, FeedbackReceiver) {
        let schedule = KeySchedule::with_frames_per_key([9u8; KEY_LEN], 1_000);
        (
            FeedbackSender::new(schedule.cipher_for_feedback()),
            FeedbackReceiver::new(schedule.cipher_for_feedback()),
        )
    }

    #[test]
    fn feedback_is_sealed_and_opened_under_a_key_of_its_own() {
        let (mut sender, mut receiver) = ciphers();

        let mut buffer = [0u8; MAX_FEEDBACK_LEN];
        let len = sender
            .seal(
                &Feedback::Nack {
                    frame_index: 12,
                    fragments: vec![3, 4],
                },
                &mut buffer,
            )
            .unwrap();

        assert_eq!(
            receiver.open(&buffer[..len]),
            Ok(Some(Feedback::Nack {
                frame_index: 12,
                fragments: vec![3, 4],
            }))
        );
        assert_eq!(receiver.unauthenticated(), 0);
    }

    /// The feedback key must not be a media key: the media nonce is `(frame_index,
    /// fragment_index)`, and a feedback message carries neither, so sharing a key would mean two
    /// different messages under one nonce.
    #[test]
    fn the_feedback_key_is_not_a_media_key() {
        let schedule = KeySchedule::with_frames_per_key([3u8; KEY_LEN], 100);
        assert_ne!(schedule.key_for(0), schedule.feedback_key());
        assert_ne!(schedule.key_for(1), schedule.feedback_key());
    }

    #[test]
    fn a_replayed_nack_is_refused_rather_than_acted_on() {
        let (mut sender, mut receiver) = ciphers();

        let mut buffer = [0u8; MAX_FEEDBACK_LEN];
        let len = sender
            .seal(&Feedback::RequestKeyframe { newest_frame: 1 }, &mut buffer)
            .unwrap();

        assert!(receiver.open(&buffer[..len]).unwrap().is_some());
        assert_eq!(
            receiver.open(&buffer[..len]).unwrap(),
            None,
            "a captured NACK replayed at the sender is a retransmit storm anyone can drive"
        );
        assert_eq!(receiver.replays(), 1);
    }

    #[test]
    fn a_forged_or_corrupt_datagram_is_refused() {
        let (mut sender, mut receiver) = ciphers();

        let mut buffer = [0u8; MAX_FEEDBACK_LEN];
        let len = sender
            .seal(&Feedback::StreamReset { last_presented: 5 }, &mut buffer)
            .unwrap();

        let mut tampered = buffer[..len].to_vec();
        let last = tampered.len() - 1;
        tampered[last] ^= 0x01;

        assert_eq!(
            receiver.open(&tampered),
            Err(FeedbackOpenError::Unauthenticated)
        );
        assert_eq!(receiver.unauthenticated(), 1);
    }

    #[test]
    fn out_of_order_feedback_within_the_window_is_still_read() {
        let (mut sender, mut receiver) = ciphers();
        let mut buffer = [0u8; MAX_FEEDBACK_LEN];

        let mut sealed = Vec::new();
        for index in 0..5u64 {
            let len = sender
                .seal(
                    &Feedback::Nack {
                        frame_index: index,
                        fragments: vec![0],
                    },
                    &mut buffer,
                )
                .unwrap();
            sealed.push(buffer[..len].to_vec());
        }

        // Delivered 0, 2, 1, 4, 3 — a reorder a UDP link produces for free.
        for index in [0usize, 2, 1, 4, 3] {
            let opened = receiver.open(&sealed[index]).unwrap();
            if index == 1 || index == 3 {
                // Older than the highest already seen: authentic, and correctly ignored.
                assert_eq!(opened, None);
            } else {
                assert!(opened.is_some(), "message {index} was lost to a reorder");
            }
        }
    }
}
