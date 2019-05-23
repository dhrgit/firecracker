// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//

use std::io::{Read, Write};
use std::num::Wrapping;
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;

use super::super::defs::uapi;
use super::super::packet::VsockPacket;
use super::super::{Result as VsockResult, VsockError};
use super::super::{VsockChannel, VsockEpollListener};
use super::txbuf::TxBuf;
use super::{defs as unix_defs, Error, Result};

#[derive(Debug, PartialEq)]
pub enum ConnState {
    LocalInit,
    PeerInit,
    Established,
    LocalClosed,
    PeerClosed(bool, bool),
    Killed,
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
        Self {
            data: it.into_mask(),
        }
    }
}

pub struct VsockConnection {
    state: ConnState,

    peer_cid: u64,
    local_port: u32,
    peer_port: u32,
    stream: UnixStream,

    tx_buf: Option<TxBuf>,

    fwd_cnt: Wrapping<u32>,
    peer_buf_alloc: u32,
    peer_fwd_cnt: Wrapping<u32>,

    // Total bytes sent to peer (guest vsock driver)
    rx_cnt: Wrapping<u32>,

    // Last fwd_cnt sent to peer
    last_fwd_cnt_to_peer: Wrapping<u32>,

    pending_rx: PendingRxSet,
    muxer_flags: u16,
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
            tx_buf: None,
            fwd_cnt: Wrapping(0),
            peer_buf_alloc,
            peer_fwd_cnt: Wrapping(0),
            rx_cnt: Wrapping(0),
            last_fwd_cnt_to_peer: Wrapping(0),
            pending_rx: PendingRxSet::from(PendingRx::Response),
            muxer_flags: 0,
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
            tx_buf: None,
            fwd_cnt: Wrapping(0),
            peer_buf_alloc: 0,
            peer_fwd_cnt: Wrapping(0),
            rx_cnt: Wrapping(0),
            last_fwd_cnt_to_peer: Wrapping(0),
            pending_rx: PendingRxSet::from(PendingRx::Request),
            muxer_flags: 0,
        }
    }

    pub fn is_shutting_down(&self) -> bool {
        !self.pending_rx.contains(PendingRx::Rst)
            && match self.state {
                ConnState::PeerClosed(true, true) | ConnState::LocalClosed => true,
                _ => false,
            }
    }

    pub fn get_muxer_flag(&self, flag: u16) -> bool {
        self.muxer_flags & flag != 0
    }

    pub fn set_muxer_flag(&mut self, flag: u16) -> bool {
        let res = self.get_muxer_flag(flag);
        self.muxer_flags |= flag;
        res
    }

    pub fn clear_muxer_flag(&mut self, flag: u16) -> bool {
        let res = self.get_muxer_flag(flag);
        self.muxer_flags &= !flag;
        res
    }

    pub fn kill(&mut self) {
        self.state = ConnState::Killed;
        self.pending_rx.insert(PendingRx::Rst);
    }

    fn send_bytes(&mut self, buf: &[u8]) -> Result<()> {
        if self.has_data_in_tx_buf() {
            let tx_buf = self.tx_buf.as_mut().unwrap();
            self.fwd_cnt += Wrapping(tx_buf.flush(&mut self.stream)? as u32);
            if tx_buf.is_empty() {
                self.send_bytes(buf)?;
            } else {
                tx_buf.push(buf)?;
            }
            return Ok(());
        }

        let written = self.stream.write(buf).map_err(Error::IoError)?;
        self.fwd_cnt += Wrapping(written as u32);

        if written < buf.len() {
            #[allow(clippy::redundant_closure)]
            self.tx_buf
                .get_or_insert_with(|| TxBuf::new())
                .push(&buf[written..])?;
        }

        Ok(())
    }

    fn peer_needs_credit_update(&self) -> bool {
        (self.fwd_cnt - self.last_fwd_cnt_to_peer).0 as usize
            >= unix_defs::CONN_CREDIT_UPDATE_THRESHOLD
    }

    fn need_credit_update_from_peer(&self) -> bool {
        self.peer_avail_credit() == 0
    }

    fn peer_avail_credit(&self) -> usize {
        (Wrapping(self.peer_buf_alloc as u32) - (self.rx_cnt - self.peer_fwd_cnt)).0 as usize
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
        pkt.hdr.buf_alloc = unix_defs::CONN_TX_BUF_SIZE as u32;
        pkt.hdr.fwd_cnt = self.fwd_cnt.0;
        pkt
    }

    fn has_data_in_tx_buf(&self) -> bool {
        self.tx_buf.is_some() && !self.tx_buf.as_ref().unwrap().is_empty()
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
                } else {
                    pkt.set_op(uapi::VSOCK_OP_RW).set_len(read_cnt as u32);
                }
            }
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
                if pkt.hdr.op == uapi::VSOCK_OP_RW =>
            {
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
                    self.kill();
                    return Ok(());
                }
                if self.peer_needs_credit_update() {
                    self.pending_rx.insert(PendingRx::CreditUpdate);
                }
            }

            ConnState::LocalInit if pkt.hdr.op == uapi::VSOCK_OP_RESPONSE => {
                self.send_bytes(format!("OK {}\n", self.local_port).as_bytes())
                    .and_then(|_| {
                        self.state = ConnState::Established;
                        Ok(())
                    })
                    .unwrap_or_else(|_| {
                        self.kill();
                    });
            }

            ConnState::Established if pkt.hdr.op == uapi::VSOCK_OP_SHUTDOWN => {
                let recv_off = pkt.hdr.flags & uapi::VSOCK_FLAGS_SHUTDOWN_RCV != 0;
                let send_off = pkt.hdr.flags & uapi::VSOCK_FLAGS_SHUTDOWN_SEND != 0;
                self.state = ConnState::PeerClosed(recv_off, send_off);
                if recv_off && send_off && !self.has_data_in_tx_buf() {
                    self.pending_rx.insert(PendingRx::Rst);
                }
            }

            ConnState::PeerClosed(ref mut recv_off, ref mut send_off)
                if pkt.hdr.op == uapi::VSOCK_OP_SHUTDOWN =>
            {
                *recv_off = *recv_off || (pkt.hdr.flags & uapi::VSOCK_FLAGS_SHUTDOWN_RCV != 0);
                *send_off = *send_off || (pkt.hdr.flags & uapi::VSOCK_FLAGS_SHUTDOWN_SEND != 0);
                if *recv_off && *send_off && !self.has_data_in_tx_buf() {
                    self.pending_rx.insert(PendingRx::Rst);
                }
            }

            ConnState::Established | ConnState::PeerInit | ConnState::PeerClosed(false, _)
                if pkt.hdr.op == uapi::VSOCK_OP_CREDIT_UPDATE =>
            {
                // Nothing to do here; we've already updated peer credit.
            }

            ConnState::Established | ConnState::PeerInit | ConnState::PeerClosed(_, false)
                if pkt.hdr.op == uapi::VSOCK_OP_CREDIT_REQUEST =>
            {
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

    fn has_pending_rx(&self) -> bool {
        !self.pending_rx.is_empty()
    }
}

impl VsockEpollListener for VsockConnection {
    fn get_polled_fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }

    fn get_polled_evset(&self) -> epoll::Events {
        let mut evset = epoll::Events::empty();
        if self.has_data_in_tx_buf() {
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
            if !self.has_data_in_tx_buf() {
                info!("vsock: unexpected EPOLLOUT event received");
                return;
            }
            let flushed = self
                .tx_buf
                .as_mut()
                .unwrap()
                .flush(&mut self.stream)
                .unwrap_or_else(|err| {
                    info!(
                        "vsock: error flushing TX buf for (lp={}, pp={}): {:?}",
                        self.local_port, self.peer_port, err
                    );
                    self.kill();
                    0
                });
            self.fwd_cnt += Wrapping(flushed as u32);
        }
    }
}
