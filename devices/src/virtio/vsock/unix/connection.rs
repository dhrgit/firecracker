// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//

use std::io::{Read, Write, ErrorKind};
use std::num::Wrapping;
use std::os::unix::net::UnixStream;
use std::os::unix::io::{AsRawFd, RawFd};

use super::super::packet::{VsockPacket};
use super::super::defs::uapi;
use super::super::{Result as VsockResult, VsockError};
use super::super::{VsockChannel, VsockEpollListener};
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

    pub fn has_pending_rx(&self) -> bool {
        !self.pending_rx.is_empty()
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

    fn init_pkt<'a>(&self, pkt: &'a mut VsockPacket) -> &'a mut VsockPacket {
        for b in pkt.hdr.as_mut_slice() {
            *b = 0;
        }
        pkt.hdr.src_cid = uapi::VSOCK_HOST_CID;
        pkt.hdr.dst_cid = self.peer_cid;
        pkt.hdr.src_port = self.local_port;
        pkt.hdr.dst_port = self.peer_port;
        pkt.hdr.type_ = uapi::VSOCK_TYPE_STREAM;
        pkt.hdr.buf_alloc = self.buf_alloc;
        pkt.hdr.fwd_cnt = self.fwd_cnt.0;
        pkt
    }

}

impl VsockChannel for VsockConnection {

    fn recv_pkt(&mut self, pkt: &mut VsockPacket) -> VsockResult<()> {

        self.init_pkt(pkt);

        if self.pending_rx.remove(PendingRx::Rst) {
            pkt.set_op(uapi::VSOCK_OP_RST);
            return Ok(());
        }

        if self.pending_rx.remove(PendingRx::Response) {
            self.state = ConnState::Established;
            pkt.set_op(uapi::VSOCK_OP_RESPONSE);
            return Ok(());
        }

        if self.pending_rx.remove(PendingRx::Request) {
            pkt.set_op(uapi::VSOCK_OP_REQUEST);
            return Ok(());
        }

        if self.pending_rx.remove(PendingRx::CreditUpdate) && !self.has_pending_rx() {
            pkt.set_op(uapi::VSOCK_OP_CREDIT_UPDATE);
            self.last_fwd_cnt_to_peer = self.fwd_cnt;
            return Ok(());
        }

        self.pending_rx.remove(PendingRx::Rw);

        match self.state {
            ConnState::Established | ConnState::PeerClosed(false, _) => (),
            _ => {
                pkt.set_op(uapi::VSOCK_OP_RST);
                return Ok(());
            }
        }

        if self.need_credit_update_from_peer() {
            self.last_fwd_cnt_to_peer = self.fwd_cnt;
            pkt.set_op(uapi::VSOCK_OP_CREDIT_REQUEST);
            return Ok(());
        }

        // It's safe to unwrap here, since an RX packet will always have a buffer (this is ensured
        // by `VsockPacket::from_rx_virtq_head()`).
        //
        let buf = pkt.buf.as_mut().unwrap();

        let max_len = std::cmp::min(buf.as_slice().len(), self.peer_avail_credit());

        match self.stream.read(&mut buf.as_mut_slice()[..max_len]) {
            Ok(read_cnt) => {
                if read_cnt == 0 {
                    self.state = ConnState::LocalClosed;
                    pkt.set_op(uapi::VSOCK_OP_SHUTDOWN)
                        .set_flag(uapi::VSOCK_FLAGS_SHUTDOWN_RCV)
                        .set_flag(uapi::VSOCK_FLAGS_SHUTDOWN_SEND);
                }
                else {
                    pkt.set_op(uapi::VSOCK_OP_RW)
                        .set_len(read_cnt as u32);
                }
            },
            Err(e) => {
                if e.kind() == std::io::ErrorKind::WouldBlock {
                    return Err(VsockError::NoData);
                }
                pkt.set_op(uapi::VSOCK_OP_RST);
            }
        };

        self.rx_cnt += Wrapping(pkt.hdr.len);
        self.last_fwd_cnt_to_peer = self.fwd_cnt;

        Ok(())
    }


    fn send_pkt(&mut self, pkt: &VsockPacket) -> VsockResult<()> {

        self.peer_buf_alloc = pkt.hdr.buf_alloc;
        self.peer_fwd_cnt = Wrapping(pkt.hdr.fwd_cnt);

        match self.state {

            ConnState::Established | ConnState::PeerClosed(_, false)
            if pkt.hdr.op == uapi::VSOCK_OP_RW => {
                if pkt.buf.is_none() {
                    info!(
                        "vsock: dropping empty data packet from guest (lp={}, pp={}",
                        self.local_port, self.peer_port
                    );
                    return Ok(());
                }
                let buf_slice = &pkt.buf.as_ref().unwrap().as_slice()[..(pkt.hdr.len as usize)];
                if let Err(err) = self.send_bytes(buf_slice) {
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
                    self.state, *pkt.hdr
                );
            }
        };

        Ok(())
    }

}

impl VsockEpollListener for VsockConnection {

    fn get_polled_fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }

    fn get_polled_evset(&self) -> epoll::Events {
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

    fn notify(&mut self, evset: epoll::Events) {
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

}
