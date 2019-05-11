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

#[derive(Clone, Copy, PartialEq)]
enum PendingRx {
    Request = 0,
    Response = 1,
    Rst = 2,
    Rw = 3,
    CreditUpdate = 4,
}
impl PendingRx {
    fn into_mask(self) -> u16 {
        1u16 << (self as u16)
    }
}

struct PendingRxSet {
    data: u16,
}
impl PendingRxSet {
    fn insert(&mut self, it: PendingRx) {
        self.data |= it.into_mask();
    }
    fn remove(&mut self, it: PendingRx) -> bool {
        let ret = self.contains(it);
        self.data &= !it.into_mask();
        ret
    }
    fn contains(&self, it: PendingRx) -> bool {
        self.data & it.into_mask() != 0
    }
    fn is_empty(&self) -> bool {
        self.data == 0
    }
}
impl From<PendingRx> for PendingRxSet {
    fn from(it: PendingRx) -> Self {
        Self { data: it.into_mask() }
    }
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

    pending_rx: PendingRxSet,
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
            pending_rx: PendingRxSet::from(PendingRx::Response),
        }
    }

    pub fn new_local_init(
        stream: UnixStream,
        peer_cid: u64,
        local_port: u32,
        peer_port: u32,
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
            pending_rx: PendingRxSet::from(PendingRx::Request)
        }
    }

    pub fn send_pkt(&mut self, pkt: VsockPacket) {

        self.peer_buf_alloc = pkt.hdr.buf_alloc;
        self.peer_fwd_cnt = Wrapping(pkt.hdr.fwd_cnt);

        match self.state {

            ConnState::Established | ConnState::PeerClosed(_, false)
            if pkt.hdr.op == uapi::VSOCK_OP_RW => {
                if let Err(err) = self.send_bytes(pkt.buf.as_slice()) {
                    info!(
                        "vsock: error writing to local stream (lp={}, pp={}): {:?}",
                        self.local_port, self.peer_port, err
                    );
                    self.pending_rx.insert(PendingRx::Rst);
                }
                if self.peer_needs_credit_update() {
                    self.pending_rx.insert(PendingRx::CreditUpdate);
                }
            },

            ConnState::LocalInit if pkt.hdr.op == uapi::VSOCK_OP_RESPONSE => {
                self.state = ConnState::Established;
            },

            ConnState::Established
            if pkt.hdr.op == uapi::VSOCK_OP_SHUTDOWN => {
                let recv_off = pkt.hdr.flags & uapi::VSOCK_FLAGS_SHUTDOWN_RCV != 0;
                let send_off = pkt.hdr.flags & uapi::VSOCK_FLAGS_SHUTDOWN_SEND != 0;
                self.state = ConnState::PeerClosed(recv_off, send_off);
                if recv_off && send_off && self.tx_buf.len() == 0 {
                    self.pending_rx.insert(PendingRx::Rst);
                }
            },

            ConnState::PeerClosed(ref mut recv_off, ref mut send_off)
            if pkt.hdr.op == uapi::VSOCK_OP_SHUTDOWN => {
                *recv_off = *recv_off || (pkt.hdr.flags & uapi::VSOCK_FLAGS_SHUTDOWN_RCV != 0);
                *send_off = *send_off || (pkt.hdr.flags & uapi::VSOCK_FLAGS_SHUTDOWN_SEND != 0);
                if *recv_off && *send_off && self.tx_buf.len() == 0 {
                    self.pending_rx.insert(PendingRx::Rst);
                }
            },

            ConnState::Established | ConnState::PeerInit | ConnState::PeerClosed(false, _)
            if pkt.hdr.op == uapi::VSOCK_OP_CREDIT_UPDATE => {
                // Nothing to do here; we've already updated peer credit.
            },

            ConnState::Established | ConnState::PeerInit | ConnState::PeerClosed(_, false)
            if pkt.hdr.op == uapi::VSOCK_OP_CREDIT_REQUEST => {
                self.pending_rx.insert(PendingRx::CreditUpdate);
            }

            _ => {
                debug!(
                    "vsock: dropping invalid TX pkt for connection: state={:?}, pkt.hdr={:?}",
                    self.state, pkt.hdr
                );
            }
        };
    }

    pub fn recv_pkt(&mut self, mut max_len: usize) -> Option<VsockPacket> {

        debug!(
            "vsock: conn.recv_pkt, max_len={}, state={:?}, pending_rx={:04x}",
            max_len, self.state, self.pending_rx.data
        );
        if self.pending_rx.remove(PendingRx::Rst) {
            return Some(self.new_pkt_for_peer(uapi::VSOCK_OP_RST, 0, None));
        }

        if self.pending_rx.remove(PendingRx::Response) {
            self.state = ConnState::Established;
            return Some(self.new_pkt_for_peer(uapi::VSOCK_OP_RESPONSE, 0, None));
        }

        if self.pending_rx.remove(PendingRx::Request) {
            return Some(self.new_pkt_for_peer(uapi::VSOCK_OP_REQUEST, 0, None));
        }

        if self.pending_rx.remove(PendingRx::CreditUpdate) && !self.has_pending_rx() {
            return Some(self.new_pkt_for_peer(uapi::VSOCK_OP_CREDIT_UPDATE, 0, None));
        }

        self.pending_rx.remove(PendingRx::Rw);

        match self.state {
            ConnState::Established | ConnState::PeerClosed(false, _) => (),
            _ => {
                return Some(self.new_pkt_for_peer(uapi::VSOCK_OP_RST, 0, None));
            }
        }

        if self.need_credit_update_from_peer() {
            self.last_fwd_cnt_to_peer = self.fwd_cnt;
            return Some(self.new_pkt_for_peer(uapi::VSOCK_OP_CREDIT_REQUEST, 0, None));
        }

        // `max_len` only tells us how much space the driver has made available in an RX buffer.
        // We also need to consider how much credit the peer has left for this stream.
        max_len = std::cmp::min(max_len, self.peer_avail_credit());

        // Beyond this point, we'll need to produce a data packet. No point in trying that
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
                    self.new_pkt_for_peer(
                        uapi::VSOCK_OP_SHUTDOWN,
                        uapi::VSOCK_FLAGS_SHUTDOWN_RCV | uapi::VSOCK_FLAGS_SHUTDOWN_SEND,
                        None
                    )
                }
                else {
                    buf.resize(read_cnt, 0);
                    self.new_pkt_for_peer(uapi::VSOCK_OP_RW, 0, Some(buf))
                }
            },
            Err(e) => {
                if e.kind() == std::io::ErrorKind::WouldBlock {
                    return None;
                }
                self.new_pkt_for_peer(uapi::VSOCK_OP_RST, 0, None)
            }
        };

        self.rx_cnt += Wrapping(pkt.hdr.len);
        self.last_fwd_cnt_to_peer = self.fwd_cnt;
        Some(pkt)
    }

    pub fn has_pending_rx(&self) -> bool {
        !self.pending_rx.is_empty()
    }

    pub fn as_raw_fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }

    pub fn get_polled_evset(&self) -> epoll::Events {
        let mut evset = epoll::Events::empty();
        if self.tx_buf.len() > 0 {
            evset.insert(epoll::Events::EPOLLOUT);
        }
        match self.state {
            ConnState::LocalClosed | ConnState::PeerClosed(true, _) => (),
            _ if self.need_credit_update_from_peer() => (),
            _ => evset.insert(epoll::Events::EPOLLIN),
        }
        evset
    }

    pub fn get_polled_fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }

    pub fn notify(&mut self, evset: epoll::Events) {
        if evset.contains(epoll::Events::EPOLLIN) {
            self.pending_rx.insert(PendingRx::Rw);
        }
        if evset.contains(epoll::Events::EPOLLOUT) {
            self.flush_tx_buf()
                .unwrap_or_else(|err| {
                    info!(
                        "vsock: error flushing TX buf for (lp={}, pp={}): {:?}",
                        self.local_port, self.peer_port, err
                    );
                    self.pending_rx.insert(PendingRx::Rst);
                    false
                });
        }
    }

    // true -> tx buf empty
    // false -> some data still in tx buf
    fn send_bytes(&mut self, buf: &[u8]) -> Result<bool> {
        // TODO: clean this up
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
    fn flush_tx_buf(&mut self) -> Result<bool> {

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


    fn peer_needs_credit_update(&self) -> bool {
        // TODO: un-hardcode and clean this up
        (self.fwd_cnt - self.last_fwd_cnt_to_peer).0 > (self.buf_alloc - 8192)
    }

    fn need_credit_update_from_peer(&self) -> bool {
        self.peer_avail_credit() == 0
    }

    fn new_pkt_for_peer(&self, op: u16, flags: u32, buf: Option<Vec<u8>>) -> VsockPacket {
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

