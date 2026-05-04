//! Fixed-capacity byte ring buffer for TTY I/O queues.
//!
//! `TtyRing<N>` is a statically-allocated power-of-two-or-arbitrary-N ring.
//! No heap allocation. Used for the canonical line buffer (`cooked_buf` in
//! `LdiscState`), the input queue, and the output queue.

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Error returned when pushing to a full ring.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RingFull;

// ---------------------------------------------------------------------------
// Ring buffer
// ---------------------------------------------------------------------------

/// Fixed-capacity byte ring buffer.
///
/// Capacity is a const generic parameter; the buffer is stored inline in the
/// struct, requiring no heap allocation and no `alloc` dependency.
pub struct TtyRing<const N: usize> {
    buf: [u8; N],
    /// Index of the next byte to read (consumer cursor).
    head: usize,
    /// Index of the next byte to write (producer cursor).
    tail: usize,
    len: usize,
}

impl<const N: usize> TtyRing<N> {
    /// Create an empty ring.
    pub const fn new() -> Self {
        Self {
            buf: [0u8; N],
            head: 0,
            tail: 0,
            len: 0,
        }
    }

    // -----------------------------------------------------------------------
    // Push / pop
    // -----------------------------------------------------------------------

    /// Push one byte at the tail. Returns `Err(RingFull)` if the buffer is full.
    pub fn push(&mut self, byte: u8) -> Result<(), RingFull> {
        if self.len == N {
            return Err(RingFull);
        }
        self.buf[self.tail] = byte;
        self.tail = (self.tail + 1) % N;
        self.len += 1;
        Ok(())
    }

    /// Push one byte at the head.
    ///
    /// Used to restore bytes that were speculatively removed for a transport
    /// kick but not accepted by the driver. This preserves FIFO order.
    pub fn push_front(&mut self, byte: u8) -> Result<(), RingFull> {
        if self.len == N {
            return Err(RingFull);
        }
        self.head = (self.head + N - 1) % N;
        self.buf[self.head] = byte;
        self.len += 1;
        Ok(())
    }

    /// Pop one byte from the head. Returns `None` if the ring is empty.
    pub fn pop(&mut self) -> Option<u8> {
        if self.len == 0 {
            return None;
        }
        let byte = self.buf[self.head];
        self.head = (self.head + 1) % N;
        self.len -= 1;
        Some(byte)
    }

    /// Return the next byte that would be popped, without consuming it.
    pub fn front(&self) -> Option<u8> {
        if self.len == 0 {
            None
        } else {
            Some(self.buf[self.head])
        }
    }

    /// Remove the most recently pushed byte (pop from the tail).
    ///
    /// Used by VERASE to delete the last character accumulated in the
    /// canonical buffer without disturbing earlier characters.
    pub fn pop_back(&mut self) -> Option<u8> {
        if self.len == 0 {
            return None;
        }
        self.tail = (self.tail + N - 1) % N;
        let byte = self.buf[self.tail];
        self.len -= 1;
        Some(byte)
    }

    // -----------------------------------------------------------------------
    // Bulk operations
    // -----------------------------------------------------------------------

    /// Push bytes from a slice. Returns the number of bytes actually pushed.
    ///
    /// Stops as soon as the ring becomes full; remaining bytes are not pushed.
    pub fn extend_from_slice(&mut self, bytes: &[u8]) -> usize {
        let mut count = 0;
        for &b in bytes {
            if self.push(b).is_err() {
                break;
            }
            count += 1;
        }
        count
    }

    /// Drain all bytes from `self` into `out`. Returns the number of bytes moved.
    ///
    /// Stops when `self` is empty or `out` is full. In the latter case the
    /// undrained bytes remain in `self`.
    pub fn drain_into<const M: usize>(&mut self, out: &mut TtyRing<M>) -> usize {
        let mut count = 0;
        loop {
            if out.is_full() || self.is_empty() {
                break;
            }
            if let Some(b) = self.pop() {
                let _ = out.push(b);
                count += 1;
            }
        }
        count
    }

    /// Drain bytes into a plain output slice.
    ///
    /// Returns the number of bytes copied. Stops when either the ring is empty
    /// or the slice is full.
    pub fn drain_to_slice(&mut self, out: &mut [u8]) -> usize {
        let mut count = 0;
        for slot in out {
            let Some(byte) = self.pop() else {
                break;
            };
            *slot = byte;
            count += 1;
        }
        count
    }

    // -----------------------------------------------------------------------
    // Inspection
    // -----------------------------------------------------------------------

    /// Number of bytes currently in the ring.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Ring capacity.
    pub const fn capacity(&self) -> usize {
        N
    }

    /// Number of bytes that can be pushed before the ring is full.
    pub const fn space(&self) -> usize {
        N - self.len
    }

    /// True if the ring contains no bytes.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// True if the ring has no space remaining.
    pub const fn is_full(&self) -> bool {
        self.len == N
    }

    /// Discard all bytes.
    pub fn clear(&mut self) {
        self.head = 0;
        self.tail = 0;
        self.len = 0;
    }
}

impl<const N: usize> Default for TtyRing<N> {
    fn default() -> Self {
        Self::new()
    }
}
