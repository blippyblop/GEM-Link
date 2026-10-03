//! Where datagrams come from and where they go, as two traits.
//!
//! The crate docs say "no sockets", and that is right: a media *plane* that knows about
//! `socket2` cannot be driven by a recording, a bench, or a test. What was missing is the other
//! half of that decision. With only the receiving side abstracted, the sender could be exercised
//! against a recording and the receiver against a replay, but **never against each other** — and
//! "the sender and the receiver disagree about what was sent" is exactly the class of defect that
//! survives that arrangement. The two are now one [`DatagramSink`] and one [`DatagramSource`], so a
//! test can put them in the same room.
//!
//! Nothing here knows about clocks. A datagram is enqueued when the caller says so and read when
//! the caller asks; latency, reorder and loss are the caller's model, because that is the only way
//! a link can be measured deterministically instead of waited for.

use std::{cell::RefCell, collections::VecDeque, rc::Rc, time::Duration};

/// One read from wherever datagrams come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceEvent {
    /// `out` holds one datagram.
    Datagram,
    /// Nothing was ready within the timeout.
    Timeout,
    /// The source is finished. A replay that has run out, or a socket that will never deliver.
    Closed,
}

/// Where datagrams come from.
pub trait DatagramSource {
    fn recv(&mut self, out: &mut Vec<u8>, timeout: Duration) -> SourceEvent;
}

/// Why a datagram could not be sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkError {
    /// The kernel buffer is full. **Not** a reason to drop the frame: it is a reason to pace, and
    /// the caller is the only one who can tell the difference.
    WouldBlock,
    /// The socket is gone. Nothing more will be sent on this session.
    Closed,
}

/// Where datagrams go.
pub trait DatagramSink {
    fn send(&mut self, datagram: &[u8]) -> Result<(), SinkError>;
}

/// A sink that keeps what it was given. The bench's window into the wire.
#[derive(Debug, Default)]
pub struct Collector {
    datagrams: Vec<Vec<u8>>,
    failures: u64,
    /// Set by `fail_next` to make the next sends report a full buffer, so a caller's
    /// backpressure path can be exercised without a real socket.
    failures_pending: u64,
}

impl Collector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn datagrams(&self) -> &[Vec<u8>] {
        &self.datagrams
    }

    pub fn take(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.datagrams)
    }

    pub fn len(&self) -> usize {
        self.datagrams.len()
    }

    pub fn is_empty(&self) -> bool {
        self.datagrams.is_empty()
    }

    /// How many sends were refused. Non-zero means the caller's pacing is wrong, not that the
    /// link is bad.
    pub fn failures(&self) -> u64 {
        self.failures
    }

    pub fn fail_next(&mut self, count: u64) {
        self.failures_pending = count;
    }
}

impl DatagramSink for Collector {
    fn send(&mut self, datagram: &[u8]) -> Result<(), SinkError> {
        if self.failures_pending > 0 {
            self.failures_pending -= 1;
            self.failures += 1;
            return Err(SinkError::WouldBlock);
        }
        self.datagrams.push(datagram.to_vec());
        Ok(())
    }
}

/// A source that replays what it was given, in order.
#[derive(Debug, Default)]
pub struct Queue {
    datagrams: VecDeque<Vec<u8>>,
    closed: bool,
}

impl Queue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_datagrams(datagrams: impl IntoIterator<Item = Vec<u8>>) -> Self {
        Self {
            datagrams: datagrams.into_iter().collect(),
            closed: false,
        }
    }

    pub fn push(&mut self, datagram: Vec<u8>) {
        self.datagrams.push_back(datagram);
    }

    pub fn len(&self) -> usize {
        self.datagrams.len()
    }

    pub fn is_empty(&self) -> bool {
        self.datagrams.is_empty()
    }

    /// Make the next read report [`SourceEvent::Closed`] rather than a timeout.
    pub fn close(&mut self) {
        self.closed = true;
    }
}

impl DatagramSource for Queue {
    fn recv(&mut self, out: &mut Vec<u8>, _timeout: Duration) -> SourceEvent {
        let Some(datagram) = self.datagrams.pop_front() else {
            return if self.closed {
                SourceEvent::Closed
            } else {
                SourceEvent::Timeout
            };
        };

        out.clear();
        out.extend_from_slice(&datagram);
        SourceEvent::Datagram
    }
}

/// How a link misbehaves. Only what a two-sided unit test needs; the rich impairment model
/// (reorder, latency, stalls, a min-heap of arrival times) is `x-bench`'s, and deliberately not
/// duplicated here.
#[derive(Debug, Clone, Copy, Default)]
pub struct LinkConfig {
    /// Datagrams dropped, in parts per thousand, independently in each direction.
    pub loss_permille: u32,
}

struct Direction {
    queue: VecDeque<Vec<u8>>,
    state: u64,
    permille: u32,
    offered: u64,
    dropped: u64,
}

impl Direction {
    fn new(permille: u32, seed: u64) -> Self {
        Self {
            queue: VecDeque::new(),
            // xorshift64*, seeded. Never zero, or the generator is a fixed point.
            state: (seed ^ 0x2545_f491_4f6c_dd1d) | 1,
            permille: permille.min(1000),
            offered: 0,
            dropped: 0,
        }
    }

    fn next_random(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn push(&mut self, datagram: &[u8]) {
        self.offered += 1;
        if self.permille > 0 && (self.next_random() % 1000) < self.permille as u64 {
            self.dropped += 1;
            return;
        }
        self.queue.push_back(datagram.to_vec());
    }
}

/// One end of an [`InMemoryLink`].
pub struct Endpoint {
    out: Rc<RefCell<Direction>>,
    incoming: Rc<RefCell<Direction>>,
}

impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Endpoint")
    }
}

impl Endpoint {
    /// Datagrams this end offered to the link, and how many were dropped by it.
    pub fn offered_and_dropped(&self) -> (u64, u64) {
        let out = self.out.borrow();
        (out.offered, out.dropped)
    }

    /// Datagrams waiting to be read.
    pub fn pending(&self) -> usize {
        self.incoming.borrow().queue.len()
    }
}

impl DatagramSink for Endpoint {
    fn send(&mut self, datagram: &[u8]) -> Result<(), SinkError> {
        self.out.borrow_mut().push(datagram);
        Ok(())
    }
}

impl DatagramSource for Endpoint {
    fn recv(&mut self, out: &mut Vec<u8>, _timeout: Duration) -> SourceEvent {
        let mut incoming = self.incoming.borrow_mut();
        let Some(datagram) = incoming.queue.pop_front() else {
            return SourceEvent::Timeout;
        };
        out.clear();
        out.extend_from_slice(&datagram);
        SourceEvent::Datagram
    }
}

/// Two [`Endpoint`]s wired to each other, with independent loss per direction.
///
/// The seed is explicit and the loss is drawn from a seeded generator, so the same seed produces
/// the same losses on every machine — which is what makes a two-sided test a regression test rather
/// than a coin flip.
pub fn in_memory_link(config: LinkConfig, seed: u64) -> (Endpoint, Endpoint) {
    let a_to_b = Rc::new(RefCell::new(Direction::new(config.loss_permille, seed)));
    let b_to_a = Rc::new(RefCell::new(Direction::new(
        config.loss_permille,
        seed.rotate_left(17) ^ 0x9e37_79b9,
    )));

    (
        Endpoint {
            out: Rc::clone(&a_to_b),
            incoming: Rc::clone(&b_to_a),
        },
        Endpoint {
            out: Rc::clone(&b_to_a),
            incoming: Rc::clone(&a_to_b),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(source: &mut impl DatagramSource) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut buffer = Vec::new();
        while let SourceEvent::Datagram = source.recv(&mut buffer, Duration::ZERO) {
            out.push(buffer.clone());
        }
        out
    }

    #[test]
    fn a_collector_keeps_what_it_is_given_and_reports_refusals() {
        let mut sink = Collector::new();
        sink.send(b"one").unwrap();
        sink.fail_next(1);
        assert_eq!(sink.send(b"two"), Err(SinkError::WouldBlock));
        sink.send(b"three").unwrap();

        assert_eq!(sink.datagrams(), [b"one".to_vec(), b"three".to_vec()]);
        assert_eq!(sink.failures(), 1);
    }

    #[test]
    fn a_queue_replays_in_order_and_then_says_whether_it_is_finished() {
        let mut queue = Queue::from_datagrams([b"a".to_vec(), b"b".to_vec()]);
        let mut buffer = Vec::new();

        assert_eq!(
            queue.recv(&mut buffer, Duration::ZERO),
            SourceEvent::Datagram
        );
        assert_eq!(buffer, b"a");
        assert_eq!(
            queue.recv(&mut buffer, Duration::ZERO),
            SourceEvent::Datagram
        );
        assert_eq!(buffer, b"b");
        assert_eq!(
            queue.recv(&mut buffer, Duration::ZERO),
            SourceEvent::Timeout
        );

        queue.close();
        assert_eq!(queue.recv(&mut buffer, Duration::ZERO), SourceEvent::Closed);
    }

    #[test]
    fn a_perfect_link_delivers_everything_both_ways() {
        let (mut a, mut b) = in_memory_link(LinkConfig::default(), 1);

        a.send(b"a->b").unwrap();
        b.send(b"b->a").unwrap();

        assert_eq!(drain(&mut b), [b"a->b".to_vec()]);
        assert_eq!(drain(&mut a), [b"b->a".to_vec()]);
    }

    #[test]
    fn loss_applies_per_direction_and_is_reproducible() {
        let config = LinkConfig { loss_permille: 300 };

        let (mut a, mut b) = in_memory_link(config, 7);
        for index in 0..100u32 {
            a.send(&index.to_le_bytes()).unwrap();
        }
        b.send(b"reply").unwrap();

        let arrived = drain(&mut b);
        assert!(
            arrived.len() < 100 && arrived.len() > 40,
            "300 permille should drop roughly a third, got {} of 100",
            arrived.len()
        );

        // The other direction is drawn independently, so a single reply is not evidence, but a
        // drop here would mean the two directions share a generator.
        assert_eq!(drain(&mut a), [b"reply".to_vec()]);

        // Same seed, same losses.
        let (mut a2, mut b2) = in_memory_link(config, 7);
        for index in 0..100u32 {
            a2.send(&index.to_le_bytes()).unwrap();
        }
        assert_eq!(drain(&mut b2), arrived);
    }

    #[test]
    fn an_endpoint_knows_what_it_offered_and_what_the_link_threw_away() {
        let (mut a, _b) = in_memory_link(LinkConfig { loss_permille: 0 }, 1);
        for _ in 0..5 {
            a.send(b"x").unwrap();
        }
        assert_eq!(a.offered_and_dropped(), (5, 0));
    }
}
