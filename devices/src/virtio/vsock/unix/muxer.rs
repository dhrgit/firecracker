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
use super::VSOCK_TX_BUF_SIZE;


#[derive(Debug)]
pub enum MuxerError {
    BufferFull,
    IoError(std::io::Error),
}

pub type Result<T> = std::result::Result<T, MuxerError>;

enum MuxerDeferredRx {
    Packet(VsockPacket),
    ConnData(u32, u32),
}

enum EpollListener {
    Connection {local_port: u32, remote_port: u32, ev_set: epoll::Events},
//    IncomingConnection(UnixStream),
}

pub struct VsockMuxer {
    rxq: VecDeque<MuxerDeferredRx>,
    conn_map: HashMap<(u32, u32), VsockConnection>,
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


    fn process_event(&mut self, fd: RawFd, ev_set: epoll::Events) {
        debug!("vsock muxer: processing event ({:#?}, {:#?})", fd, ev_set);
        match self.listener_map.entry(fd) {
            Entry::Occupied(ent) => {
                match ent.get() {
                    EpollListener::Connection {
                        local_port, remote_port, ev_set: _
                    } => {
                        // send event to connection
                        // TODO: this will overflow self.rxq with events! Fix it
                        //
                        if ev_set.contains(epoll::Events::EPOLLIN) {
                            self.rxq.push_back(MuxerDeferredRx::ConnData(*local_port, *remote_port));
                        }
                        if ev_set.contains(epoll::Events::EPOLLOUT) {
                        }
                    }
                }
                ()
            },
            _ => ()
        }
    }

    fn add_connection(&mut self, conn: VsockConnection, ev_set: epoll::Events) {

        // TODO: check epoll ctl error and return error
        epoll::ctl(
            self.epoll_fd,
            epoll::ControlOptions::EPOLL_CTL_ADD,
            conn.as_raw_fd(),
            epoll::Event::new(ev_set, conn.as_raw_fd() as u64)
        ).unwrap();

        self.listener_map.insert(
            conn.as_raw_fd(),
            EpollListener::Connection {
                local_port: conn.local_port,
                remote_port: conn.remote_port,
                ev_set
            }
        );

        self.conn_map.insert((conn.local_port, conn.remote_port), conn);
    }

    fn remove_connection(&mut self, local_port: u32, remote_port: u32) {
        match self.conn_map.entry((local_port, remote_port)) {
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


    fn send_pkt(&mut self, pkt: VsockPacket) -> bool {
        let conn_key = (pkt.hdr.src_port, pkt.hdr.dst_port);

        info!("vsock: muxer.send: rxq.len = {}, hdr.op={}, hdr.len={}",
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
                warn!("vsock: dropping unexpected packet from guest, op={}", pkt.hdr.op);
                // return Err(GeneralError);
                return true;
            }

            match VsockConnection::new_local_init(pkt.hdr.src_port, pkt.hdr.dst_port) {
                Ok(mut conn) => {
                    debug!("vsock: new connection to remote:{}", pkt.hdr.dst_port);
                    conn.set_peer_credit(pkt.hdr.buf_alloc, pkt.hdr.fwd_cnt);
                    self.add_connection(conn, epoll::Events::EPOLLIN);

                    // TODO: initialize a proper packet
                    self.rxq.push_back(MuxerDeferredRx::Packet(VsockPacket::new_response(
                        uapi::VSOCK_HOST_CID,
                        self.cid,
                        pkt.hdr.dst_port,
                        pkt.hdr.src_port
                    )));
                },
                Err(e) => {
                    debug!("vsock: error establishing remote connection: {:#?}", e);

                    // TODO: initialize a proper packet
                    self.rxq.push_back(MuxerDeferredRx::Packet(VsockPacket::new_rst(
                        uapi::VSOCK_HOST_CID,
                        self.cid,
                        pkt.hdr.dst_port,
                        pkt.hdr.src_port
                    )));
                },
            }

            return true;
        }

        self.conn_map.entry(conn_key).and_modify(|conn| {
            conn.set_peer_credit(pkt.hdr.buf_alloc, pkt.hdr.fwd_cnt);
        });

        let mut update_peer_credit = false;

        match pkt.hdr.op {
            uapi::VSOCK_OP_RW => {
                let mut remove_conn = false;
                self.conn_map.entry(conn_key).and_modify(|conn| {
                    match conn.send(&pkt.buf[..pkt.hdr.len as usize]) {
                        Err(e) => {
                            debug!("vsock: error sending data on {:?}: {:?}", conn_key, e);
                            remove_conn = true;
                        },
                        Ok(complete) => {
                            // TODO: remove write event listener, if any
                            update_peer_credit = conn.peer_needs_credit_update();
                        }
                    }
                });
                if remove_conn {
                    // TODO: send RST
                    self.remove_connection(conn_key.0, conn_key.1);
                }
            },
            uapi::VSOCK_OP_SHUTDOWN => {
                // TODO: handle clean shutdown and respect read/write indications
                // TODO: emulate some kind of timeout before dropping a connection with tx_buf.len > 0
                self.rxq.push_back(MuxerDeferredRx::Packet(VsockPacket::new_rst(
                    uapi::VSOCK_HOST_CID,
                    self.cid,
                    pkt.hdr.dst_port,
                    pkt.hdr.src_port,
                )));
                self.remove_connection(conn_key.0, conn_key.1);
            },
            uapi::VSOCK_OP_RST => {
                self.remove_connection(conn_key.0, conn_key.1);
            },
            uapi::VSOCK_OP_RESPONSE => {
                // TODO: handle incoming connections
            },
            uapi::VSOCK_OP_CREDIT_REQUEST => {
                update_peer_credit = true;
                debug!("vsock: got credit request; sending update");
            },
            uapi::VSOCK_OP_CREDIT_UPDATE => {
                // Nothing to do here, we've already updated peer credit
            },
            _ => {
                warn!("vsock: dropping unexpected packet op: {}", pkt.hdr.op);
                return true;
            }
        }

        if update_peer_credit {
            let mut fwd_cnt = 0u32;
            self.conn_map.entry(conn_key).and_modify(|conn| {
                fwd_cnt = conn.fwd_cnt.0;
            });
            self.rxq.push_back(MuxerDeferredRx::Packet(VsockPacket {
                hdr: VsockPacketHdr {
                    src_cid: uapi::VSOCK_HOST_CID,
                    dst_cid: self.cid,
                    src_port: conn_key.1,
                    dst_port: conn_key.0,
                    len: 0,
                    op: uapi::VSOCK_OP_CREDIT_UPDATE,
                    flags: 0,
                    type_: uapi::VSOCK_TYPE_STREAM,
                    buf_alloc: VSOCK_TX_BUF_SIZE as u32,
                    fwd_cnt,
                    ..Default::default()
                },
                buf: Vec::new(),
            }));
        }

        true
    }

    fn recv_pkt(&mut self, max_len: usize) -> Option<VsockPacket> {

        info!("vsock: muxer.recv: rxq.len = {}", self.rxq.len());

        if let Some(drx) = self.rxq.pop_front() {
            let pkt = match drx {
                MuxerDeferredRx::Packet(pkt) => pkt,
                MuxerDeferredRx::ConnData(local_port, remote_port) => {
                    let mut buf = vec![0u8; max_len];
                    match self.conn_map.entry((local_port, remote_port)) {
                        Entry::Occupied(mut ent) => {
                            let mut conn = ent.get_mut();
                            match conn.recv(&mut buf[..]) {
                                Ok(len) => {
                                    buf.resize(len, 0);
                                    VsockPacket {
                                        hdr: VsockPacketHdr {
                                            src_cid: uapi::VSOCK_HOST_CID,
                                            dst_cid: self.cid,
                                            src_port: remote_port,
                                            dst_port: local_port,
                                            len: len as u32,
                                            op: uapi::VSOCK_OP_RW,
                                            flags: 0,
                                            type_: uapi::VSOCK_TYPE_STREAM,
                                            buf_alloc: conn.buf_alloc,
                                            fwd_cnt: conn.fwd_cnt.0,
                                            ..Default::default()
                                        },
                                        buf,
                                    }
                                },
                                Err(_) => {
                                    debug!("vsock: error reading from connection");
                                    // self.remove_connection(local_port, remote_port);
                                    // TODO: send RST to driver, remove connection
                                    return None;
                                }
                            }
                        },
                        Entry::Vacant(_ent) => {
                            debug!("Error: attempted to read from non-mapped connection");
                            return None;
                        }
                    }
                }
            };
            return Some(pkt);
        }
        None
    }

}
