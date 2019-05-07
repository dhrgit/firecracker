// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//

use std::io::{Read, Write, ErrorKind};
use std::num::Wrapping;
use std::os::unix::net::UnixStream;
use std::os::unix::io::{AsRawFd, RawFd};

use super::super::packet::{VsockPacket, VsockPacketHdr};
use super::super::defs::uapi;
use super::{Error, Result, VSOCK_TX_BUF_SIZE};


#[derive(Debug, PartialEq)]
pub enum ConnState {
    LocalInit,
    PeerInit,
    Established,
    LocalClosed,
    PeerClosed(bool, bool),
}

pub struct VsockConnection {

    state: ConnState,

    peer_cid: u64,
    local_port: u32,
    peer_port: u32,
    stream: UnixStream,

    // TODO: implement a proper ring buffer for TX
    tx_buf: Vec<u8>,

    buf_alloc: u32,
    fwd_cnt: Wrapping<u32>,
    peer_buf_alloc: u32,
    peer_fwd_cnt: Wrapping<u32>,

    // Total bytes sent to peer (guest vsock driver)
    rx_cnt: Wrapping<u32>,

    // Last fwd_cnt sent to peer
    last_fwd_cnt_to_peer: Wrapping<u32>,
}

impl VsockConnection {

    pub fn new_peer_init(
        stream: UnixStream,
        peer_cid: u64,
        local_port: u32,
        peer_port: u32,
        peer_buf_alloc: u32,
    ) -> Self {

        Self {
            peer_cid,
            local_port,
            peer_port,
            stream,
            state: ConnState::PeerInit,
            tx_buf: Vec::new(),
            buf_alloc: VSOCK_TX_BUF_SIZE as u32,
            fwd_cnt: Wrapping(0),
            peer_buf_alloc,
            peer_fwd_cnt: Wrapping(0),
            rx_cnt: Wrapping(0),
            last_fwd_cnt_to_peer: Wrapping(0),
        }
    }

    pub fn new_local_init(
        peer_cid: u64,
        local_port: u32,
        peer_port: u32,
        stream: UnixStream
    ) -> Self {
        Self {
            peer_cid,
            local_port,
            peer_port,
            stream,
            state: ConnState::LocalInit,
            tx_buf: Vec::new(),
            buf_alloc: VSOCK_TX_BUF_SIZE as u32,
            fwd_cnt: Wrapping(0),
            peer_buf_alloc: 0,
            peer_fwd_cnt: Wrapping(0),
            rx_cnt: Wrapping(0),
            last_fwd_cnt_to_peer: Wrapping(0),
        }
    }

    // true -> tx buf empty
    // false -> data waiting in tx buf
    pub fn send_pkt(&mut self, pkt: VsockPacket) -> Result<bool> {

        // TODO: finish up this state machine

        match self.state {

            ConnState::LocalInit
            if pkt.hdr.op == uapi::VSOCK_OP_RESPONSE => {
                self.state = ConnState::Established;
            },

            ConnState::Established | ConnState::PeerClosed(_, false)
            if pkt.hdr.op == uapi::VSOCK_OP_RW => {
                self.send_bytes(pkt.buf.as_slice())?;
            },

            ConnState::Established | ConnState::LocalClosed
            if pkt.hdr.op == uapi::VSOCK_OP_SHUTDOWN => {
                self.state = ConnState::PeerClosed(
                    pkt.hdr.flags & uapi::VSOCK_FLAGS_SHUTDOWN_RCV != 0,
                    pkt.hdr.flags & uapi::VSOCK_FLAGS_SHUTDOWN_SEND != 0,
                );
                // TODO: arm kill timer and RST
            },

            ConnState::PeerClosed(rcv, snd)
            if pkt.hdr.op == uapi::VSOCK_OP_SHUTDOWN => {
                self.state = ConnState::PeerClosed(
                    rcv || (pkt.hdr.flags & uapi::VSOCK_FLAGS_SHUTDOWN_RCV != 0),
                    snd || (pkt.hdr.flags & uapi::VSOCK_FLAGS_SHUTDOWN_SEND != 0),
                );
                // TODO: arm kill timer and RST
            },

            ConnState::LocalClosed => {
                // Only RST is valid in this state, and that would have been handled by the muxer.
                debug!("vsock: received bad pkt op={}; terminating connection", pkt.hdr.op);
                return Err(Error::FatalPkt);
            },

            _ if pkt.hdr.op == uapi::VSOCK_OP_CREDIT_UPDATE => {
                // Nothing to do here; will update peer credit later on.
            },

            _ if pkt.hdr.op == uapi::VSOCK_OP_CREDIT_REQUEST => {

            }

            _ => {
                // TODO: be more descriptive
                // TODO: figure out when to drop the connection and when to stomach unexpected pkts
                debug!(
                    "vsock: dropping invalid TX pkt for connection: state={:?}, op={}, len={}",
                    self.state,
                    pkt.hdr.op,
                    pkt.hdr.len,
                );
                return Ok(self.tx_buf.len() == 0);
            }
        }

        self.peer_buf_alloc = pkt.hdr.buf_alloc;
        self.peer_fwd_cnt = Wrapping(pkt.hdr.fwd_cnt);

        Ok(self.tx_buf.len() == 0)
    }

    pub fn recv_pkt(&mut self, max_len: usize) -> Option<VsockPacket> {

        // TODO: clean this up

        debug!(
            "vsock: conn.recv_pkt(): state={:?}, tx_buf.len={}",
            self.state,
            self.tx_buf.len()
        );

        if self.state == ConnState::LocalClosed
            || (self.state == ConnState::PeerClosed(true, true) && self.tx_buf.len() == 0) {
            return Some(self.build_pkt_for_peer(uapi::VSOCK_OP_RST, 0, None));
        }

        if self.state == ConnState::PeerInit {
            self.state = ConnState::Established;
            return Some(self.build_pkt_for_peer(uapi::VSOCK_OP_RESPONSE, 0, None));
        }

        if self.state == ConnState::LocalInit {
            return Some(self.build_pkt_for_peer(uapi::VSOCK_OP_REQUEST, 0, None));
        }

        // TODO: this will keep asking for a credit update from peer, until it receives one.
        if self.need_credit_update_from_peer() {
            self.last_fwd_cnt_to_peer = self.fwd_cnt;
            return Some(self.build_pkt_for_peer(uapi::VSOCK_OP_CREDIT_REQUEST, 0, None));
        }
        if self.peer_needs_credit_update() {
            self.last_fwd_cnt_to_peer = self.fwd_cnt;
            return Some(self.build_pkt_for_peer(uapi::VSOCK_OP_CREDIT_UPDATE, 0, None));
        }

        // Beyond this point, we'd need to produce a data packet. No point in trying that
        // if there's no buffer to store the data into.
        //
        if max_len == 0 {
            return None;
        }

        let mut buf: Vec<u8> = Vec::with_capacity(max_len);
        buf.resize(max_len, 0);

        let pkt = match self.stream.read(buf.as_mut_slice()) {
            Ok(read_cnt) => {
                if read_cnt == 0 {
                    self.state = ConnState::LocalClosed;
                    self.build_pkt_for_peer(
                        uapi::VSOCK_OP_SHUTDOWN,
                        uapi::VSOCK_FLAGS_SHUTDOWN_RCV | uapi::VSOCK_FLAGS_SHUTDOWN_SEND,
                        None
                    )
                }
                else {
                    buf.resize(read_cnt, 0);
                    self.build_pkt_for_peer(uapi::VSOCK_OP_RW, 0, Some(buf))
                }
            },
            Err(e) => {
                if e.kind() == std::io::ErrorKind::WouldBlock {
                    return None;
                }
                // TODO: maybe perform a graceful shutdown here as well?
                // TODO: also, is returning an RST pkt here really better than Result<Option<pkt>>?
                self.build_pkt_for_peer(uapi::VSOCK_OP_RST, 0, None)
            }
        };

        self.rx_cnt += Wrapping(pkt.hdr.len);
        self.last_fwd_cnt_to_peer = self.fwd_cnt;
        Some(pkt)
    }

    // true -> tx buf empty
    // false -> some data still in tx buf
    // TODO: do we need this return value?
    fn send_bytes(&mut self, buf: &[u8]) -> Result<bool> {

        match self.flush_tx_buf() {
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
                            Err(Error::IoError(e))
                        }
                    }
                }
            }
        }
    }


    // true -> tx buf empty
    // false -> some data still in tx buf
    pub fn flush_tx_buf(&mut self) -> Result<bool> {

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
                    return Err(Error::IoError(e));
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

    pub fn avail_credit(&self) -> usize {
        VSOCK_TX_BUF_SIZE - self.tx_buf.len()
    }

    fn peer_needs_credit_update(&self) -> bool {
        // TODO: un-hardcode and clean this up
        (self.fwd_cnt - self.last_fwd_cnt_to_peer).0 > (self.buf_alloc - 8192)
    }

    fn need_credit_update_from_peer(&self) -> bool {
        // TODO: un-hardcode and clean this up
        self.peer_avail_credit() < 8192
    }

    fn build_pkt_for_peer(&self, op: u16, flags: u32, buf: Option<Vec<u8>>) -> VsockPacket {
        VsockPacket {
            hdr: VsockPacketHdr {
                src_cid: uapi::VSOCK_HOST_CID,
                dst_cid: self.peer_cid,
                src_port: self.local_port,
                dst_port: self.peer_port,
                len: if let Some(ref b) = buf { b.len() as u32 } else { 0 },
                type_: uapi::VSOCK_TYPE_STREAM,
                op,
                flags,
                buf_alloc: self.buf_alloc,
                fwd_cnt: self.fwd_cnt.0,
                _pad: 0,
            },
            buf: buf.unwrap_or(Vec::new()),
        }
    }

    fn peer_avail_credit(&self) -> usize {
        (Wrapping(self.peer_buf_alloc as u32) - (self.rx_cnt - self.peer_fwd_cnt)).0 as usize
    }

    fn push_to_tx_buf(&mut self, buf: &[u8]) -> Result<()> {
        if self.tx_buf.len() + buf.len() > VSOCK_TX_BUF_SIZE {
            return Err(Error::BufferFull);
        }
        self.tx_buf.extend_from_slice(buf);
        Ok(())
    }

}

