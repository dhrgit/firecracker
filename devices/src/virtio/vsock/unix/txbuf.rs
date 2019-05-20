// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//

use std::io::{ErrorKind, Write};
use std::mem;
use std::num::Wrapping;

use super::defs as unix_defs;
use super::{Error, Result};


pub struct TxBuf {
    data: Box<[u8; Self::SIZE]>,
    head: Wrapping<u32>,
    tail: Wrapping<u32>,
}

impl TxBuf {

    const SIZE: usize = unix_defs::CONN_TX_BUF_SIZE;

    pub fn new() -> Self {
        Self {
            data: Box::new(unsafe {mem::uninitialized::<[u8; Self::SIZE]>()}),
            head: Wrapping(0),
            tail: Wrapping(0),
        }
    }

    pub fn len(&self) -> usize {
        (self.head - self.tail).0 as usize
    }

    pub fn push(&mut self, buf: &[u8]) -> Result<()> {
        if self.len() + buf.len() > Self::SIZE {
            return Err(Error::BufferFull);
        }

        let head_ofs = self.head.0 as usize % Self::SIZE;
        let len = std::cmp::min(Self::SIZE - head_ofs, buf.len());

        self.data[head_ofs .. (head_ofs+len)].copy_from_slice(&buf[..len]);
        if len < buf.len() {
            self.data[..(buf.len() - len)].copy_from_slice(&buf[len..]);
        }
        self.head += Wrapping(buf.len() as u32);
        Ok(())
    }

    pub fn flush<W>(&mut self, sink: &mut W) -> Result<usize>
        where W: Write
    {
        if self.is_empty() {
            return Ok(0);
        }

        let tail_ofs = self.tail.0 as usize % Self::SIZE;
        let len_to_write = std::cmp::min(Self::SIZE - tail_ofs, self.len());

        let written = match sink.write(&self.data[tail_ofs .. (tail_ofs + len_to_write)]) {
            Ok(written) => written,
            Err(e) => {
                if e.kind() == ErrorKind::WouldBlock {
                    0
                }
                else {
                    return Err(Error::IoError(e));
                }
            }
        };

        self.tail += Wrapping(written as u32);

        if written < len_to_write {
            return Ok(written);
        }

        self.flush(sink)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

