// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//


use std::collections::{HashMap, VecDeque};
use std::collections::hash_map::{Entry};
use std::os::unix::io::RawFd;

use super::super::defs::uapi;
use super::super::packet::{VsockPacket, VsockPacketHdr};
use super::super::VsockBackend;
use super::connection::VsockConnection;


#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct ConnMapKey {
    local_port: u32,
    peer_port: u32,
}

enum MuxerRx {
    Packet(VsockPacket),
    ConnRx(ConnMapKey)
}


enum EpollListener {
    Connection { key: ConnMapKey, evset: epoll::Events },
//    IncomingConnection(UnixStream),
}

pub struct VsockMuxer {
    rxq: VecDeque<MuxerRx>,
    conn_map: HashMap<ConnMapKey, VsockConnection>,
    listener_map: HashMap<RawFd, EpollListener>,
    cid: u64,
    epoll_fd: RawFd,
}

impl VsockMuxer {
    pub fn new(cid: u64, epoll_fd: RawFd) -> Self {
        Self {
            rxq: VecDeque::new(),
            conn_map: HashMap::new(),
            listener_map: HashMap::new(),
            cid,
            epoll_fd,
        }
    }


    fn process_event(&mut self, fd: RawFd, evset: epoll::Events) {
        debug!("vsock muxer: processing event ({:#?}, {:#?})", fd, evset);
        match self.listener_map.entry(fd) {
            Entry::Occupied(ent) => {
                match ent.get() {
                    EpollListener::Connection { key, .. } => {
                        // send event to connection
                        // TODO: this will overflow self.rxq with events! Fix it
                        //
                        if evset.contains(epoll::Events::EPOLLIN) {
                            self.rxq.push_back(MuxerRx::ConnRx(*key));
                        }
                        if evset.contains(epoll::Events::EPOLLOUT) {
                            // TODO: handle tx
                        }
                    }
                }
                ()
            },
            _ => ()
        }
    }

    fn add_connection(&mut self, key: ConnMapKey, conn: VsockConnection, evset: epoll::Events) {

        // TODO: check epoll ctl error and return error
        epoll::ctl(
            self.epoll_fd,
            epoll::ControlOptions::EPOLL_CTL_ADD,
            conn.as_raw_fd(),
            epoll::Event::new(evset, conn.as_raw_fd() as u64)
        ).unwrap();

        self.listener_map.insert(
            conn.as_raw_fd(),
            EpollListener::Connection { key, evset }
        );

        self.conn_map.insert(key, conn);
    }

    fn remove_connection(&mut self, key: ConnMapKey) {
        match self.conn_map.entry(key) {
            Entry::Occupied(e) => {
                let fd = e.get().as_raw_fd();
                if let Entry::Occupied(e2) = self.listener_map.entry(fd) {
                    e2.remove();
                }
                e.remove();
            },
            _ => ()
        }
    }

}

impl VsockBackend for VsockMuxer {

    fn get_epoll_listener(&self) -> (RawFd, epoll::Events) {
        (self.epoll_fd, epoll::Events::EPOLLIN)
    }

    fn notify(&mut self, _: epoll::Events) {

        debug!("vsock muxer: received kick");

        let mut epoll_events = vec![epoll::Event::new(epoll::Events::empty(), 0); 16].into_boxed_slice();
        match epoll::wait(self.epoll_fd, 0, &mut epoll_events[..]) {
            Ok(ev_cnt) => {
                for i in 0..ev_cnt {
                    self.process_event(
                        epoll_events[i].data as RawFd,
                        epoll::Events::from_bits(epoll_events[i].events).unwrap()
                    );
                }
            },
            Err(e) => {
                warn!("vsock: failed to consume muxer epoll event: {}", e);
            }
        }
    }


    // TODO: is this return value right?
    fn send_pkt(&mut self, pkt: VsockPacket) -> bool {

        let conn_key = ConnMapKey {
            local_port: pkt.hdr.dst_port,
            peer_port: pkt.hdr.src_port,
        };

        debug!("vsock: muxer.send: rxq.len = {}, hdr.op={}, hdr.len={}",
              self.rxq.len(), pkt.hdr.op, pkt.hdr.len);

        // TODO: clean up this limit (set a const, etc)
        if self.rxq.len() >= 128 {
            warn!("vsock: muxer.rxq full; refusing send()");
            return false;
        }


        // TODO: if type != stream, send RST

        // TODO: validate pkt. e.g. type = stream, cid, etc
        //

        if !self.conn_map.contains_key(&conn_key) {
            if pkt.hdr.op != uapi::VSOCK_OP_REQUEST {
                debug!("vsock: dropping unexpected packet from guest, op={}", pkt.hdr.op);
                return true;
            }

            match VsockConnection::new_peer_init(
                self.cid,
                pkt.hdr.dst_port,
                pkt.hdr.src_port,
                pkt.hdr.buf_alloc,
            ) {
                Ok(conn) => {
                    debug!("vsock: new connection to remote:{}", pkt.hdr.dst_port);
                    self.add_connection(conn_key, conn, epoll::Events::EPOLLIN);

                    // TODO: initialize a proper packet
                    self.rxq.push_back(MuxerRx::Packet(VsockPacket::new_response(
                        uapi::VSOCK_HOST_CID,
                        self.cid,
                        pkt.hdr.dst_port,
                        pkt.hdr.src_port
                    )));
                },
                Err(e) => {
                    debug!(
                        "vsock: error establishing unix connection (src={}, dst={}): {:#?}",
                        pkt.hdr.src_port,
                        pkt.hdr.dst_port,
                        e
                    );

                    // TODO: initialize a proper packet
                    self.rxq.push_back(MuxerRx::Packet(VsockPacket::new_rst(
                        uapi::VSOCK_HOST_CID,
                        self.cid,
                        pkt.hdr.dst_port,
                        pkt.hdr.src_port
                    )));
                },
            }

            return true;
        }

        if pkt.hdr.op == uapi::VSOCK_OP_RST {
            self.remove_connection(conn_key);
            return true;
        }

        // TODO: this send / recv cycle doesn't look kosher
        let mut remove_conn = false;
        let mut maybe_response = None;

        self.conn_map.entry(conn_key).and_modify(|conn| {
            match conn.send_pkt(pkt) {
                Ok(()) => {
                    // TODO: if this leads to UnixStream::read(0), something isn't right. Fix it.
                    maybe_response = conn.recv_pkt(0)
                },
                Err(err) => {
                    debug!(
                        "vsock: error sending pkt on (src={}, dst={}): {:?}",
                        conn_key.local_port,
                        conn_key.peer_port,
                        err
                    );
                    remove_conn = true;
                }
            }
        });

        if let Some(rpkt) = maybe_response {
            self.rxq.push_back(MuxerRx::Packet(rpkt));
        }
        if remove_conn {
            self.remove_connection(conn_key);
        }

        true
    }

    fn recv_pkt(&mut self, max_len: usize) -> Option<VsockPacket> {

        info!("vsock: muxer.recv: rxq.len = {}", self.rxq.len());

        let maybe_pkt = match self.rxq.pop_front() {
            Some(MuxerRx::Packet(pkt)) => {
                Some(pkt)
            },
            Some(MuxerRx::ConnRx(key)) => {
                let mut maybe_pkt: Option<VsockPacket> = None;
                self.conn_map.entry(key).and_modify(|conn| {
                    maybe_pkt = conn.recv_pkt(max_len);
                });
                maybe_pkt
            },
            None => None
        };

        // TODO: this hook looks ugly / messy
        if let Some(ref pkt) = maybe_pkt {
            if pkt.hdr.op == uapi::VSOCK_OP_RST {
                self.remove_connection(ConnMapKey {
                    local_port: pkt.hdr.src_port,
                    peer_port: pkt.hdr.dst_port,
                });
            }
        }

        maybe_pkt
    }

}
