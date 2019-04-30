// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//

use std::cmp::min;
use std::io::{Read, Write, ErrorKind};
use std::num::Wrapping;
use std::os::unix::net::UnixStream;
use std::os::unix::io::{AsRawFd, RawFd};

//use super::super::packet::VsockPacket;
use super::muxer::{MuxerError, Result};
use super::{VSOCK_TX_BUF_SIZE, TEMP_VSOCK_PATH};


pub enum VsockConnectionState {
    Established,
    RemoteClose,
    LocalClose,
}

pub struct VsockConnection {
    pub local_port: u32,
    pub remote_port: u32,
    pub stream: UnixStream,
    pub state: VsockConnectionState,

    // TODO: implement a proper ring buffer for TX
    tx_buf: Vec<u8>,

    pub buf_alloc: u32,
    pub fwd_cnt: Wrapping<u32>,
    peer_buf_alloc: u32,
    peer_fwd_cnt: Wrapping<u32>,

    // Total bytes sent to peer (guest vsock driver)
    rx_cnt: Wrapping<u32>,
}

impl VsockConnection {

    pub fn new_local_init(local_port: u32, remote_port: u32) -> Result<Self> {

        match UnixStream::connect(format!("{}_{}", TEMP_VSOCK_PATH, remote_port)) {
            Ok(stream) => {
                stream.set_nonblocking(true)
                    .map_err(MuxerError::IoError)?;
                Ok(Self {
                    local_port,
                    remote_port,
                    stream,
                    state: VsockConnectionState::Established,
                    tx_buf: Vec::with_capacity(VSOCK_TX_BUF_SIZE),
                    buf_alloc: VSOCK_TX_BUF_SIZE as u32,
                    fwd_cnt: Wrapping(0),
                    peer_buf_alloc: 0,
                    peer_fwd_cnt: Wrapping(0),
                    rx_cnt: Wrapping(0),
                })
            },
            Err(e) => Err(MuxerError::IoError(e))
        }
    }

    // true -> tx buf empty
    // false -> some data still in tx buf
    pub fn send(&mut self, buf: &[u8]) -> Result<bool> {

        match self.try_flush_tx_buf() {
            Err(e) => Err(e),
            Ok(false) => {
                self.push_to_tx_buf(buf)?;
                Ok(false)
            },
            Ok(true) => {
                match self.stream.write(buf) {
                    Ok(bytes_written) => {
                        if bytes_written < buf.len() {
                            self.push_to_tx_buf(&buf[bytes_written..])?;
                        }
                        self.fwd_cnt += Wrapping(bytes_written as u32);
                        Ok(self.tx_buf.len() == 0)
                    },
                    Err(e) => {
                        if e.kind() == ErrorKind::WouldBlock {
                            self.push_to_tx_buf(buf)?;
                            Ok(false)
                        }
                        else {
                            Err(MuxerError::IoError(e))
                        }
                    }
                }
            }
        }
    }

    // TODO: figure out a useful return value here
    pub fn recv(&mut self, buf: &mut [u8]) -> Result<usize> {

        let max_len = min(self.peer_avail_credit(), buf.len());
        let norm_buf = unsafe {
            std::slice::from_raw_parts_mut(
                buf as *mut _ as *mut u8,
                max_len
            )
        };

        match self.stream.read(norm_buf) {
            Err(e) => {
                if e.kind() == ErrorKind::WouldBlock {
                    Ok(0)
                }
                else {
                    Err(MuxerError::IoError(e))
                }
            },
            Ok(bytes_read) => {
                if bytes_read == 0 {
                    self.state = VsockConnectionState::RemoteClose;
                }
                self.rx_cnt += Wrapping(bytes_read as u32);
                Ok(bytes_read)
            },
        }
    }

    // true -> tx buf empty
    // false -> some data still in tx buf
    pub fn try_flush_tx_buf(&mut self) -> Result<bool> {

        if self.tx_buf.len() == 0 {
            return Ok(true);
        }

        let written = match self.stream.write(&self.tx_buf) {
            Ok(bytes) => bytes,
            Err(e) => {
                if e.kind() == ErrorKind::WouldBlock {
                    return Ok(false);
                }
                else {
                    return Err(MuxerError::IoError(e));
                }
            }
        };

        self.tx_buf = self.tx_buf.split_off(written);
        self.fwd_cnt += Wrapping(written as u32);

        Ok(self.tx_buf.len() == 0)
    }

    pub fn as_raw_fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }

    pub fn set_peer_credit(&mut self, peer_buf_alloc: u32, peer_fwd_cnt: u32) {
        // TODO: simplify / clean up this wrapping nonsense
        self.peer_buf_alloc = peer_buf_alloc;
        self.peer_fwd_cnt = Wrapping(peer_fwd_cnt);
    }

    pub fn avail_credit(&self) -> usize {
        VSOCK_TX_BUF_SIZE - self.tx_buf.len()
    }

    pub fn peer_needs_credit_update(&self) -> bool {
        // TODO: detect this properly
        (self.fwd_cnt.0 % self.buf_alloc) > (self.buf_alloc - 8192)
    }

    fn peer_avail_credit(&self) -> usize {
        (Wrapping(self.peer_buf_alloc as u32) - (self.rx_cnt - self.peer_fwd_cnt)).0 as usize
    }

    fn push_to_tx_buf(&mut self, buf: &[u8]) -> Result<()> {
        if self.tx_buf.len() + buf.len() > VSOCK_TX_BUF_SIZE {
            return Err(MuxerError::BufferFull);
        }
        self.tx_buf.extend_from_slice(buf);
        Ok(())
    }

}

