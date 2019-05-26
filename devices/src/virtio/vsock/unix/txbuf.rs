// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//

use std::io::{ErrorKind, Write};
use std::mem;
use std::num::Wrapping;

use super::defs as unix_defs;
use super::{Error, Result};

/// A simple ring-buffer implementation, used by vsock connections to buffer TX (guest -> host) data.
/// Memory for this buffer is allocated lazily, since buffering will only be needed when the host
/// can't read fast enough.
///
pub struct TxBuf {
    /// The actual u8 buffer - only allocated after the first push.
    data: Option<Box<[u8; Self::SIZE]>>,
    /// Ring-buffer head offset - where new data is pushed to.
    head: Wrapping<u32>,
    /// Ring-buffer tail offset - where data is flushed from.
    tail: Wrapping<u32>,
}

impl TxBuf {
    /// Total buffer size, in bytes.
    ///
    const SIZE: usize = unix_defs::CONN_TX_BUF_SIZE;

    /// Ring-buffer constructor.
    ///
    pub fn new() -> Self {
        Self {
            data: None,
            head: Wrapping(0),
            tail: Wrapping(0),
        }
    }

    /// Get the used length of this buffer - number of bytes that have been pushed in, but not yet
    /// flushed out.
    ///
    pub fn len(&self) -> usize {
        (self.head - self.tail).0 as usize
    }

    /// Push a byte slice onto the ring-buffer.
    ///
    /// Either the entire source slice will be pushed to the ring-buffer, or none of it, if there
    /// isn't enough room, in which case `Err(Error::BufferFull)` is returned.
    ///
    pub fn push(&mut self, src: &[u8]) -> Result<()> {
        // Error out if there's no room to push the entire slice.
        if self.len() + src.len() > Self::SIZE {
            return Err(Error::BufferFull);
        }

        // We're using a closure here to return the boxed slice, instead of a value
        // (i.e. `get_or_insert_with()` instead of `get_or_insert()`), because we only want the box
        // created when `self.data` is None. If we were to use `get_or_insert(box)`, the box argument
        // would always get evaluated (which implies a heap allocation), even though it would later
        // be discarded (when `self.data.is_some()`).
        // Apparently, clippy fails to see this, and insists on issuing some warning.
        #[allow(clippy::redundant_closure)]
        let data = self.data.get_or_insert_with(||
                // Using uninitialized memory here is quite safe, since we never read from any area
                // of the buffer before writing to it. First we push, then we flush only what had
                // been prviously pushed.
                Box::new(unsafe {mem::uninitialized::<[u8; Self::SIZE]>()}));

        // Buffer head, as an offset into the data slice.
        let head_ofs = self.head.0 as usize % Self::SIZE;

        // Pushing a slice to this buffer can take either one or two slice copies:
        // - one copy, if the slice fits between `head_ofs` and `Self::SIZE`; or
        // - two copies, if the ring-buffer head wraps around.

        // First copy length: we can only go from the head offset up to the total buffer size.
        let len = std::cmp::min(Self::SIZE - head_ofs, src.len());
        data[head_ofs..(head_ofs + len)].copy_from_slice(&src[..len]);

        // If the slice didn't fit, the buffer head will wrap around, and pushing continues from
        // the start of the buffer (`&self.data[0]`).
        if len < src.len() {
            data[..(src.len() - len)].copy_from_slice(&src[len..]);
        }

        // Either way, we've just pushed exactly `src.len()` bytes, so that's the amount by which
        // the (wrapping) buffer head needs to move forward.
        self.head += Wrapping(src.len() as u32);

        Ok(())
    }

    /// Flush the contents of the ring-buffer to a writable stream.
    ///
    /// Return the number of bytes that have been transferred out of the ring-buffer and into the
    /// writable stream.
    ///
    pub fn flush_to<W>(&mut self, sink: &mut W) -> Result<usize>
    where
        W: Write,
    {
        // Nothing to do, if this buffer holds no data.
        if self.is_empty() {
            return Ok(0);
        }

        // Buffer tail, as an offset into the buffer data slice.
        let tail_ofs = self.tail.0 as usize % Self::SIZE;

        // Flushing the buffer can take either one or two writes:
        // - one write, if the tail doesn't need to wrap around to reach the head; or
        // - two writes, if the tail would wrap around: tail to slice end, then slice end to head.

        // First write length: the lesser of tail to slice end, or tail to head.
        let len_to_write = std::cmp::min(Self::SIZE - tail_ofs, self.len());

        // It's safe to unwrap here, since we've already checked if the buffer was empty.
        let data = self.data.as_ref().unwrap();

        // Issue the first write and absorb any `WouldBlock` error (we can just try again later).
        let written = match sink.write(&data[tail_ofs..(tail_ofs + len_to_write)]) {
            Ok(written) => written,
            Err(e) => {
                if e.kind() == ErrorKind::WouldBlock {
                    0
                } else {
                    return Err(Error::IoError(e));
                }
            }
        };

        // Move the buffer tail ahead by the amount (of bytes) we were able to flush out.
        self.tail += Wrapping(written as u32);

        // If we weren't able to flush out as much as we tried, there's no point in attempting our
        // second write.
        if written < len_to_write {
            return Ok(written);
        }

        // Attempt our second write. This will return immediately if a second write isn't needed,
        // since checking for an empty buffer is the first thing we do in this function.
        Ok(written + self.flush_to(sink)?)
    }

    /// Check if the buffer holds any data that hasn't yet been flushed out.
    ///
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
