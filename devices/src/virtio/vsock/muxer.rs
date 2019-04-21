// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//


use std::collections::{HashMap, VecDeque};
use std::collections::hash_map::{Entry, OccupiedEntry, VacantEntry};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::io::{AsRawFd, RawFd};

use super::packet::VsockPacket;
use super::*;

enum VsockConnectionState {
    LocalInit,
    RemoteInit,
    Established,
    RemoteClose,
}

struct VsockConnection {
    local_port: u32,
    remote_port: u32,
    remote: Option<UnixStream>,
    state: VsockConnectionState,
}

impl VsockConnection {

    fn new_local_init(local_port: u32, remote_port: u32) -> Self {
        match UnixStream::connect(format!("{}_{}", TEMP_VSOCK_PATH, remote_port)) {
            Ok(remote) => Self {
                local_port,
                remote_port,
                remote: Some(remote),
                state: VsockConnectionState::Established,
            },
            Err(e) => {
                info!("vsock: failed connecting to remote port {}: {:?}", remote_port, e);
                Self {
                    local_port,
                    remote_port,
                    remote: None,
                    state: VsockConnectionState::RemoteClose,
                }
            }
        }
    }

    fn send(&mut self, buf: Box<[u8]>) -> Result<()> {
        match self.remote {
            None => Err(VsockError:: GeneralError),
            Some(ref mut stream) => {
                let wcnt = stream.write(&buf).map_err(|_| VsockError::GeneralError)?;
                if wcnt != buf.len() {
                    Err(VsockError::GeneralError)
                }
                else {
                    Ok(())
                }
            }
        }
    }

    pub fn recv(&mut self, buf: &mut [u8]) -> Result<usize> {
        self.remote.as_ref().unwrap().read(buf).map_err(|_| VsockError::GeneralError)
        //Err(VsockError::GeneralError)
    }

}
struct MuxerDeferredRx {
    op: u16,
    local_port: u32,
    remote_port: u32,
}

enum EpollListener {
    Connection {local_port: u32, remote_port: u32, ev_set: epoll::Events},
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

    pub fn send(&mut self, pkt: VsockPacket) {
        let conn_key = (pkt.hdr.src_port, pkt.hdr.dst_port);

        // TODO: validate pkt. e.g. type = stream, cid, etc
        //

        if !self.conn_map.contains_key(&conn_key) {
            if pkt.hdr.op != VSOCK_OP_REQUEST {
                // TODO: maybe send a RST back?
                warn!("vsock: dropping unexpected packet from guest, op={}", pkt.hdr.op);
                // return Err(GeneralError);
                return;
            }

            let conn = VsockConnection::new_local_init(pkt.hdr.src_port, pkt.hdr.dst_port);
            match conn.state {
                VsockConnectionState::Established => {
                    debug!("vsock: new connection to remote:{}", pkt.hdr.dst_port);
                    self.add_connection(conn, epoll::Events::EPOLLIN);
                    self.rxq.push_back(MuxerDeferredRx {
                        op: VSOCK_OP_RESPONSE,
                        local_port: pkt.hdr.src_port,
                        remote_port: pkt.hdr.dst_port,
                    });
                },
                VsockConnectionState::RemoteClose => {
                    self.rxq.push_back(MuxerDeferredRx {
                        op: VSOCK_OP_RST,
                        local_port: pkt.hdr.src_port,
                        remote_port: pkt.hdr.src_port,
                    });
                },
                _ => {
                    error!("vsock: unexpected remote response");
                }
            }
        }

        match pkt.hdr.op {
            VSOCK_OP_RW => {
                self.conn_map.entry(conn_key).and_modify(|conn| {
                    conn.send(pkt.buf.into_boxed_slice());
                });
            },
            VSOCK_OP_SHUTDOWN => {
                // TODO: handle clean shutdown and respect read/write indications
                self.rxq.push_back(MuxerDeferredRx {
                    op: VSOCK_OP_RST,
                    local_port: pkt.hdr.src_port,
                    remote_port: pkt.hdr.dst_port,
                });
                // TODO: close remote connection
                self.conn_map.remove(&conn_key);
            },
            VSOCK_OP_RST => {
                // TODO: close remote connection
                self.conn_map.remove(&conn_key);
            },
            VSOCK_OP_RESPONSE => {

            },
            VSOCK_OP_CREDIT_REQUEST => {
                // TODO: implement credit update, in reply to request
            },
            VSOCK_OP_CREDIT_UPDATE => {
                // TODO: implment conn credit update
            },
            _ => {
                warn!("vsock: dropping unexpected packet op: {}", pkt.hdr.op);
                return;
            }
        }
    }

    pub fn recv(&mut self, max_len: usize) -> Option<VsockPacket> {
        // self.rxq.pop_front()
        if let Some(drx) = self.rxq.pop_front() {
            let pkt = match drx.op {
                VSOCK_OP_RST => {
                    VsockPacket::new_rst(
                        VSOCK_HOST_CID,
                        self.cid,
                        drx.remote_port,
                        drx.local_port,
                    )
                },
                VSOCK_OP_RESPONSE => {
                    VsockPacket::new_response(
                        VSOCK_HOST_CID,
                        self.cid,
                        drx.remote_port,
                        drx.local_port,
                    )
                },
                VSOCK_OP_RW => {
                    let mut buf = vec![0u8; max_len];
                    match self.conn_map.entry((drx.local_port, drx.remote_port)) {
                        Entry::Occupied(mut ent) => {
                            match ent.get_mut().recv(&mut buf[..]) {
                                Ok(len) => {
                                    buf.resize(len, 0);
                                    VsockPacket::new_rw(
                                        VSOCK_HOST_CID,
                                        self.cid,
                                        drx.remote_port,
                                        drx.local_port,
                                        buf
                                    )
                                },
                                Err(e) => {
                                    debug!("Error reading from connection");
                                    return None;
                                }
                            }

                        },
                        Entry::Vacant(e) => {
                            debug!("Error: attempted to read from non-mapped connection");
                            return None;
                        }
                    }

                }
                _ => return None,
            };
            return Some(pkt);
        }
        None
    }

    pub fn kick(&mut self) {

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

    fn process_event(&mut self, fd: RawFd, ev_set: epoll::Events) {
        debug!("vsock muxer: processing event ({:#?}, {:#?})", fd, ev_set);
        match self.listener_map.entry(fd) {
            Entry::Occupied(ent) => {
                match ent.get() {
                    EpollListener::Connection {
                        local_port, remote_port, ev_set: _
                    } => {
                        // send event to connection
                        if ev_set.contains(epoll::Events::EPOLLIN) {
                            self.rxq.push_back(MuxerDeferredRx {
                                op: VSOCK_OP_RW,
                                local_port: *local_port,
                                remote_port: *remote_port,
                            });
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
        let remote_fd = conn.remote.as_ref().unwrap().as_raw_fd();

        // TODO: check epoll ctl error and return error
        epoll::ctl(
            self.epoll_fd,
            epoll::ControlOptions::EPOLL_CTL_ADD,
            remote_fd,
            epoll::Event::new(ev_set, remote_fd as u64)
        ).unwrap();
        self.listener_map.insert(
            remote_fd,
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
                let fd = e.get().remote.as_ref().unwrap().as_raw_fd();
                if let Entry::Occupied(e2) = self.listener_map.entry(fd) {
                    e2.remove();
                }
                e.remove();
            },
            _ => ()
        }
    }

}

