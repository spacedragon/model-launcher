//! Process supervision primitives (`docs/architecture.md` §4, §8).
//!
//! This module owns the engine-neutral building blocks the Job 6 supervisor
//! needs around a managed engine process. It currently provides
//! [`ByteTailRing`], the bounded stdout/stderr tail buffer used when a child
//! is launched in its own process group (§4 step 4) and consulted when its
//! failure must be classified (§6: the "stderr tail" input to
//! [`FailureClass`]).
//!
//! [`ByteTailRing`] is deliberately synchronous and allocation-free on the
//! append path: a supervisor pump can call [`ByteTailRing::append`] from any
//! task without touching the async runtime, and the ring itself never fails —
//! only construction does.

/// The ring was configured with a zero byte capacity, which would retain no
/// bytes and make every snapshot empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("byte tail ring capacity must be at least 1 byte")]
pub enum ByteTailRingError {
    /// A zero capacity was passed to [`ByteTailRing::new`].
    ZeroCapacity,
}

/// A bounded ring buffer retaining the most recent [`Self::capacity`] bytes
/// of an unbounded byte stream (e.g. a child process's stdout or stderr).
///
/// Determinism properties:
///
/// - **Bounded memory.** At most `capacity` bytes are ever retained; earlier
///   bytes are overwritten in place once the ring is full, so a chatty or
///   pathological child can never grow memory.
/// - **Exact byte counting.** [`Self::total_bytes`] counts every byte ever
///   appended, including bytes already evicted, so the supervisor can tell
///   "the child emitted 1 MiB" apart from "the tail holds 1 MiB".
/// - **Deterministic snapshots.** [`Self::snapshot`] returns the chronological
///   tail of the retained bytes (oldest to newest, wrapped order) for any
///   request of the last `n` bytes; the same append sequence always yields
///   the same snapshot.
///
/// The type is `Clone` because a snapshot is a plain byte sequence; cloning
/// copies the retained bytes and the counters, never any child state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ByteTailRing {
    /// Maximum number of retained bytes; always at least 1.
    capacity: usize,
    /// The retained bytes. While the ring is still filling, `buf` holds the
    /// stream head-to-tail; once full, `buf[start]` is the oldest retained
    /// byte and `buf[head]` is the slot the next byte overwrites.
    buf: Vec<u8>,
    /// Index of the oldest retained byte once the ring is full.
    start: usize,
    /// Number of bytes written into `buf` so far in the fill phase, or the
    /// next overwrite slot once the ring is full.
    head: usize,
    /// Total bytes appended, evicted or not.
    total: u64,
}

impl ByteTailRing {
    /// Create a ring retaining up to `capacity` most recent bytes.
    ///
    /// # Errors
    ///
    /// [`ByteTailRingError::ZeroCapacity`] when `capacity` is 0, because a
    /// zero-capacity ring is useless (it retains nothing and reports every
    /// snapshot as empty) and almost always a configuration bug.
    pub fn new(capacity: usize) -> Result<Self, ByteTailRingError> {
        if capacity == 0 {
            return Err(ByteTailRingError::ZeroCapacity);
        }
        Ok(Self {
            capacity,
            buf: Vec::with_capacity(capacity),
            start: 0,
            head: 0,
            total: 0,
        })
    }

    /// Append `bytes` to the stream, evicting oldest retained bytes as
    /// needed to stay within the capacity. Never fails and never reallocates
    /// once the ring is full.
    pub fn append(&mut self, bytes: &[u8]) {
        let mut rest = bytes;
        while !rest.is_empty() {
            // Fill phase: `buf` holds the head of the stream and `head`
            // marks where the next byte lands.
            if self.head == self.buf.len() {
                if self.buf.len() == self.capacity {
                    // The fill phase just completed; continue in full mode.
                    self.head = 0;
                    continue;
                }
                let space = self.capacity - self.buf.len();
                let take = space.min(rest.len());
                self.buf.extend_from_slice(&rest[..take]);
                self.head = self.buf.len();
                self.total += take as u64;
                rest = &rest[take..];
                continue;
            }
            // Full mode: every slot holds a byte. Each overwritten slot
            // becomes the newest, and the oldest advances by the same count.
            let take = (self.capacity - self.head).min(rest.len());
            self.buf[self.head..self.head + take].copy_from_slice(&rest[..take]);
            self.total += take as u64;
            self.start = (self.start + take).rem_euclid(self.capacity);
            self.head = (self.head + take).rem_euclid(self.capacity);
            rest = &rest[take..];
        }
    }

    /// Snapshot of the last `n` bytes of the stream, oldest to newest.
    ///
    /// When fewer than `n` bytes are retained (including an empty ring), the
    /// whole retained tail is returned; the result is therefore at most
    /// `n` bytes and never empty unless nothing was ever appended.
    #[must_use]
    pub fn snapshot(&self, n: usize) -> Vec<u8> {
        let retained = if self.head == self.buf.len() {
            self.buf.len()
        } else {
            self.capacity
        };
        let take = retained.min(n);
        if take == 0 {
            return Vec::new();
        }
        let first = (self.start + (retained - take)).rem_euclid(self.capacity);
        let mut out = Vec::with_capacity(take);
        if first + take <= self.capacity {
            out.extend_from_slice(&self.buf[first..first + take]);
        } else {
            let before_wrap = self.capacity - first;
            out.extend_from_slice(&self.buf[first..]);
            out.extend_from_slice(&self.buf[..take - before_wrap]);
        }
        out
    }

    /// The configured retention capacity in bytes.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Number of bytes currently retained (at most [`Self::capacity`]).
    #[must_use]
    pub fn len(&self) -> usize {
        if self.head == self.buf.len() {
            self.buf.len()
        } else {
            self.capacity
        }
    }

    /// Whether no bytes have ever been appended.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.total == 0
    }

    /// Whether the ring retains its full capacity (i.e. the stream was longer
    /// than the capacity).
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.len() == self.capacity
    }

    /// Total bytes appended so far, including bytes already evicted.
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.total
    }

    /// Drop all retained bytes and counters, returning to the fresh state.
    pub fn clear(&mut self) {
        self.buf.clear();
        self.start = 0;
        self.head = 0;
        self.total = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring(capacity: usize) -> ByteTailRing {
        match ByteTailRing::new(capacity) {
            Ok(ring) => ring,
            Err(error) => panic!("expected a valid ring of capacity {capacity}, got {error:?}"),
        }
    }

    #[test]
    fn rejects_a_zero_capacity() {
        assert!(matches!(
            ByteTailRing::new(0),
            Err(ByteTailRingError::ZeroCapacity)
        ));
    }

    #[test]
    fn a_zero_capacity_error_names_the_rule() {
        let message = ByteTailRingError::ZeroCapacity.to_string();
        assert!(
            message.contains("at least 1 byte"),
            "unexpected error message: {message}"
        );
    }

    #[test]
    fn a_fresh_ring_is_empty_and_reports_zero_bytes() {
        let ring = ring(16);
        assert!(ring.is_empty());
        assert!(!ring.is_full());
        assert_eq!(ring.len(), 0);
        assert_eq!(ring.total_bytes(), 0);
        assert!(ring.snapshot(16).is_empty());
    }

    #[test]
    fn appending_below_capacity_retains_everything_in_order() {
        let mut ring = ring(16);
        ring.append(b"hello ");
        ring.append(b"world");
        assert_eq!(ring.len(), 11);
        assert_eq!(ring.total_bytes(), 11);
        assert!(!ring.is_full());
        assert_eq!(ring.snapshot(16), b"hello world");
    }

    #[test]
    fn a_partial_snapshot_returns_the_newest_bytes_of_the_tail() {
        let mut ring = ring(16);
        ring.append(b"hello world");
        assert_eq!(ring.snapshot(5), b"world");
        assert_eq!(ring.snapshot(0), b"");
    }

    #[test]
    fn a_snapshot_larger_than_the_retained_bytes_returns_the_whole_tail() {
        let mut ring = ring(4);
        ring.append(b"abc");
        assert_eq!(ring.snapshot(100), b"abc");
    }

    #[test]
    fn appending_exactly_the_capacity_fills_the_ring_once() {
        let mut ring = ring(4);
        ring.append(b"abcd");
        assert!(ring.is_full());
        assert_eq!(ring.total_bytes(), 4);
        assert_eq!(ring.snapshot(4), b"abcd");
    }

    #[test]
    fn a_single_oversized_chunk_keeps_its_own_tail() {
        let mut ring = ring(4);
        ring.append(b"0123456789");
        assert_eq!(ring.total_bytes(), 10);
        assert_eq!(ring.len(), 4);
        assert_eq!(ring.snapshot(4), b"6789");
    }

    #[test]
    fn overflow_wraps_and_keeps_the_newest_bytes() {
        let mut ring = ring(4);
        ring.append(b"abcd");
        ring.append(b"efgh");
        assert_eq!(ring.total_bytes(), 8);
        assert_eq!(ring.snapshot(4), b"efgh");
    }

    #[test]
    fn an_overflow_chunk_crossing_the_wrap_boundary_is_kept_in_order() {
        // "abcd" fills the ring; "ef" overwrites the first two slots, so the
        // retained bytes wrap around the buffer end: C D (tail) then E F.
        let mut ring = ring(4);
        ring.append(b"abcd");
        ring.append(b"ef");
        assert_eq!(ring.total_bytes(), 6);
        assert_eq!(ring.snapshot(4), b"cdef");
    }

    #[test]
    fn repeated_small_appends_older_than_the_capacity_evict_in_order() {
        let mut ring = ring(3);
        for &byte in b"abcdef" {
            ring.append(&[byte]);
        }
        assert_eq!(ring.total_bytes(), 6);
        assert_eq!(ring.snapshot(3), b"def");
        assert_eq!(ring.snapshot(5), b"def");
    }

    #[test]
    fn the_one_byte_capacity_keeps_only_the_last_byte() {
        let mut ring = ring(1);
        ring.append(b"x");
        ring.append(b"y");
        ring.append(b"z");
        assert_eq!(ring.total_bytes(), 3);
        assert_eq!(ring.snapshot(1), b"z");
    }

    #[test]
    fn a_chunk_exactly_the_ring_size_in_full_mode_replaces_everything() {
        let mut ring = ring(4);
        ring.append(b"abcd");
        ring.append(b"1234"); // one full turn: start and head land back home
        assert_eq!(ring.total_bytes(), 8);
        assert_eq!(ring.snapshot(4), b"1234");
        ring.append(b"567890"); // another full turn plus two extra bytes
        assert_eq!(ring.total_bytes(), 14);
        assert_eq!(ring.snapshot(4), b"7890");
    }

    #[test]
    fn snapshots_are_stable_across_further_appends_of_the_same_history() {
        // Two rings receiving identical append sequences snapshot identically:
        // determinism is a function of the append sequence, not of timing.
        let mut a = ring(7);
        let mut b = ring(7);
        let chunks: [&[u8]; 5] = [b"alpha", b"beta", b"gamma", b"delta", b"epsilon"];
        for ring in [&mut a, &mut b] {
            for chunk in chunks {
                ring.append(chunk);
            }
        }
        assert_eq!(a.snapshot(7), b.snapshot(7));
        assert_eq!(a.total_bytes(), b.total_bytes());
        // 26 bytes were appended; a request above that returns the whole
        // retained tail, and both rings agree on it.
        assert_eq!(a.snapshot(20), b"epsilon");
        assert_eq!(a.snapshot(20), b.snapshot(20));
    }

    #[test]
    fn a_long_stream_keeps_the_exact_trailing_bytes() {
        let capacity = 1000;
        let mut ring = ring(capacity);
        let total = 10_000usize;
        let mut stream = Vec::with_capacity(total);
        for index in 0..total {
            stream.push(u8::try_from(index % 251).expect("a remainder below 251 fits in a byte"));
        }
        // Append in awkward chunk sizes to exercise fill, wrap and partial
        // wrap paths over many iterations.
        let mut rest = stream.as_slice();
        while !rest.is_empty() {
            let take = rest.len().min(977);
            ring.append(&rest[..take]);
            rest = &rest[take..];
        }
        assert_eq!(ring.total_bytes(), total as u64);
        assert_eq!(ring.len(), capacity);
        let expected = stream[total - capacity..].to_vec();
        assert_eq!(ring.snapshot(capacity), expected);
        assert_eq!(ring.snapshot(37), stream[total - 37..].to_vec());
    }

    #[test]
    fn an_empty_append_is_a_noop() {
        let mut ring = ring(4);
        ring.append(b"ab");
        ring.append(b"");
        assert_eq!(ring.total_bytes(), 2);
        assert_eq!(ring.snapshot(4), b"ab");
    }

    #[test]
    fn clearing_resets_retention_and_counters() {
        let mut ring = ring(4);
        ring.append(b"abcdefgh");
        ring.clear();
        assert!(ring.is_empty());
        assert!(!ring.is_full());
        assert_eq!(ring.len(), 0);
        assert_eq!(ring.total_bytes(), 0);
        assert!(ring.snapshot(4).is_empty());
        // The ring is reusable after a clear.
        ring.append(b"xyz");
        assert_eq!(ring.total_bytes(), 3);
        assert_eq!(ring.snapshot(4), b"xyz");
    }

    #[test]
    fn a_clone_observes_the_same_history() {
        let mut ring = ring(4);
        ring.append(b"abcdef");
        let clone = ring.clone();
        assert_eq!(clone.total_bytes(), ring.total_bytes());
        assert_eq!(clone.snapshot(4), ring.snapshot(4));
        // The clone is independent: further appends do not cross over.
        ring.append(b"gh");
        assert_ne!(clone.snapshot(4), ring.snapshot(4));
        assert_eq!(clone.snapshot(4), b"cdef");
        assert_eq!(ring.snapshot(4), b"efgh");
    }

    #[test]
    fn the_retained_bytes_are_always_within_capacity() {
        // Property: no matter how the stream arrives, `len` never exceeds
        // the capacity and the snapshot never exceeds what was retained.
        for capacity in [1usize, 2, 3, 5, 64, 1024] {
            let mut ring = ring(capacity);
            let mut stream = Vec::new();
            let mut next = 0u32;
            for index in 0..3000usize {
                let chunk_len = 1 + (index % 7);
                let chunk: Vec<u8> = (0..chunk_len)
                    .map(|_| {
                        next = next.wrapping_mul(31).wrapping_add(7);
                        u8::try_from(next % 256).expect("a u32 remainder fits in a byte")
                    })
                    .collect();
                stream.extend_from_slice(&chunk);
                ring.append(&chunk);
                assert!(ring.len() <= capacity);
                let snap = ring.snapshot(capacity * 2);
                assert!(snap.len() <= capacity);
                assert_eq!(
                    snap,
                    stream[stream.len().saturating_sub(capacity)..].to_vec()
                );
            }
        }
    }
}
