// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//


use std::collections::{HashMap, HashSet, VecDeque};
use std::collections::hash_map::{Entry};
use std::io::{Read, ErrorKind};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::io::{RawFd, AsRawFd};

use super::super::defs::uapi;
use super::super::packet::{VsockPacket};
use super::super::VsockBackend;
use super::connection::VsockConnection;
use super::{Error, Result, TEMP_VSOCK_PATH};


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
    RxSock,
    FutureConn(UnixStream),
}

pub struct VsockMuxer {
    rxq: VecDeque<MuxerRx>,
    conn_map: HashMap<ConnMapKey, VsockConnection>,
    listener_map: HashMap<RawFd, EpollListener>,
    cid: u64,
    epoll_fd: RawFd,
    rx_sock: UnixListener,
    local_port_set: HashSet<u32>,
    local_port_last: u32,
}

impl VsockMuxer {
    pub fn new(cid: u64, epoll_fd: RawFd) -> Self {
        let mut muxer = Self {
            rxq: VecDeque::new(),
            conn_map: HashMap::new(),
            listener_map: HashMap::new(),
            cid,
            epoll_fd,
            rx_sock: UnixListener::bind(TEMP_VSOCK_PATH).unwrap(),
            local_port_last: (1u32 << 31),
            local_port_set: HashSet::new(),
        };

        // TODO: have rx_sock provided by the VMM, before seccomp application
        // and get rid of these unwraps.

        epoll::ctl(
            muxer.epoll_fd,
            epoll::ControlOptions::EPOLL_CTL_ADD,
            muxer.rx_sock.as_raw_fd(),
            epoll::Event::new(epoll::Events::EPOLLIN, muxer.rx_sock.as_raw_fd() as u64)
        ).unwrap();

        muxer.rx_sock.set_nonblocking(true).unwrap();
        muxer.listener_map.insert(
            muxer.rx_sock.as_raw_fd(),
            EpollListener::RxSock
        );
        muxer
    }


    fn process_event(&mut self, fd: RawFd, evset: epoll::Events) {
        debug!("vsock muxer: processing event ({:#?}, {:#?})", fd, evset);

        enum Action {
            AddFutureConn(UnixStream),
            HandleConnEvent(ConnMapKey),
            UpgradeFutureConn(u32),
            RemoveFutureConn,
            Nop
        }
        let mut action = Action::Nop;

        {
            match self.listener_map.get_mut(&fd)  {
                Some(EpollListener::Connection {key, ..}) => {
                    action = Action::HandleConnEvent(*key);
                },
                Some(EpollListener::RxSock) => {
                    if let Ok((stream, _))  = self.rx_sock.accept() {
                        debug!("vsock: accepting local connection");
                        if stream.set_nonblocking(true).is_ok() {
                            action = Action::AddFutureConn(stream);
                        }
                    }
                    else {
                        debug!("vsock: error accept()ing local connection");
                    }
                },
                Some(EpollListener::FutureConn(ref mut stream)) => {
                    let mut buf = vec![0u8; 64];
                    match stream.read(buf.as_mut_slice()) {
                        Ok(0) => {
                            debug!("vsock: local connection closed unexpectedly");
                            action = Action::RemoveFutureConn;
                        },
                        Ok(read_cnt) => {
                            debug!("vsock: received data on local connection: {:?}", buf);
                            buf.resize(read_cnt, 0);
                            match std::str::from_utf8(buf.as_slice()) {
                                Ok(s) => {
                                    match s.trim().parse::<u32>() {
                                        Ok(n) => {
                                            debug!("vsock: got port number on local conn: {}", n);
                                            action = Action::UpgradeFutureConn(n);
                                        },
                                        Err(e) => debug!("vsock: error parsing port no: {:?}", e),
                                    }
                                }
                                Err(e) => {
                                    debug!("vsock: utf8 error parsing incoming command: {:?}", e);
                                }
                            }
                        },
                        Err(e) => {
                            if e.kind() != ErrorKind::WouldBlock {
                                action = Action::RemoveFutureConn;
                            }
                        }
                    }
                },
                None => ()
            }
        }

        match action {
            Action::HandleConnEvent(key) => {
                if evset.contains(epoll::Events::EPOLLIN) {
                    self.rxq.push_back(MuxerRx::ConnRx(key));
                }
                if evset.contains(epoll::Events::EPOLLOUT) {
                    let mut unreg_tx: bool = false;
                    let mut remove_conn: bool = false;
                    {
                        self.conn_map.entry(key).and_modify(|conn| {
                            unreg_tx = match conn.try_flush_tx_buf() {
                                Ok(drained) => drained,
                                Err(e) => {
                                    debug!(
                                        "vsock: error flushing TX buffer for connection (s={}, d={})",
                                        key.local_port, key.peer_port
                                    );
                                    remove_conn = true;
                                    false
                                }
                            }
                        });
                    }
                    if remove_conn {
                        self.remove_connection(key);
                    }
                    else {
                        // TODO: register/unregister EPOLLOUT here. Also check/add EPOLLOUT after send_pkt()
                        // Also, clean up this messy code. Break it up into functions, maybe
                        // add an impl EpollListener for modifying listener evset in-place.
                        if unreg_tx {
                        }
                    }
                }
            },
            Action::AddFutureConn(stream) => {
                self.add_listener(
                    stream.as_raw_fd(),
                    EpollListener::FutureConn(stream)
                ).unwrap_or_else(|e| {
                    debug!("vsock muxer: error adding connection: {:?}", e);
                });
            },
            Action::RemoveFutureConn => {
                self.remove_listener(fd);
            },
            Action::UpgradeFutureConn(peer_port) => {

                let local_port = match self.allocate_local_port() {
                    Some(p) => p,
                    None => {
                        error!("vsock muxer: unable to allocate new local port");
                        // This will also drop the FutureConn
                        self.remove_listener(fd);
                        return;
                    }
                };
                let conn_key = ConnMapKey { local_port, peer_port };
                if let Some(EpollListener::FutureConn(stream)) = self.remove_listener(fd) {
                    let conn = VsockConnection::new_local_init(
                        self.cid,
                        local_port,
                        peer_port,
                        stream
                    );
                    self.add_connection(conn_key, conn, epoll::Events::EPOLLIN);
                    self.rxq.push_back(MuxerRx::ConnRx(conn_key));
                }
                else {
                    // TODO: We can only get here due to some coding error, so we should probably panic
                }
            },
            Action::Nop => ()
        }

    }

    fn add_connection(&mut self, key: ConnMapKey, conn: VsockConnection, evset: epoll::Events) {

        self.add_listener(
            conn.as_raw_fd(),
            EpollListener::Connection {key, evset}
        ).and_then(|_| {
            self.conn_map.insert(key, conn);
            Ok(())
        }).unwrap_or_else(|e| {
            debug!("vsock muxer: error adding listener: {:?}", e);
        });

    }

    fn remove_connection(&mut self, key: ConnMapKey) {
        let fd;
        {
            fd = match self.conn_map.get(&key) {
                Some(conn) => conn.as_raw_fd(),
                None => return,
            };
        }
        self.remove_listener(fd);

        {
            if let Entry::Occupied(ent) = self.conn_map.entry(key) {
                ent.remove();
            }
        }
        self.free_local_port(key.local_port);
    }

    fn add_listener(&mut self, fd: RawFd, listener: EpollListener) -> Result<()> {

        let evset = match listener {
            EpollListener::Connection {evset, ..} => evset,
            EpollListener::FutureConn(_) => epoll::Events::EPOLLIN,
            EpollListener::RxSock => epoll::Events::EPOLLIN,
        };


        // TODO: do we need this ctl_mod here?
        if self.listener_map.contains_key(&fd) {
            epoll::ctl(
                self.epoll_fd,
                epoll::ControlOptions::EPOLL_CTL_MOD,
                fd,
                epoll::Event::new(evset, fd as u64),
            ).or_else(|e| {
                self.listener_map.remove(&fd);
                Err(e)
            }).map_err(Error::EpollCtl)?;
            self.listener_map.entry(fd)
                .and_modify(|old_listener| *old_listener = listener);
        }
        else {
            epoll::ctl(
                self.epoll_fd,
                epoll::ControlOptions::EPOLL_CTL_ADD,
                fd,
                epoll::Event::new(evset, fd as u64),
            ).and_then(|_| {
                self.listener_map.insert(fd, listener);
                Ok(())
            }).map_err(Error::EpollCtl)?;

        }

        Ok(())
    }

    fn remove_listener(&mut self, fd: RawFd) -> Option<EpollListener> {

        epoll::ctl(
           self.epoll_fd,
            epoll::ControlOptions::EPOLL_CTL_DEL,
            fd,
            epoll::Event::new(epoll::Events::empty(), 0)
        ).unwrap_or_else(|e| {
            if e.kind() != ErrorKind::NotFound {
                // TODO: EPOLL_CTL_DEL should only fail with ENOENT on a valid epoll fd
                // should we panic?
            }
        });

        self.listener_map.remove(&fd)
    }

    fn allocate_local_port(&mut self) -> Option<u32> {

        // TODO: this doesn't seem very efficient.
        // Rewrite this to limit port range and use a bitmap.
        //

        loop {
            self.local_port_last = if self.local_port_last == std::u32::MAX {
                1u32 << 31
            }
            else {
                self.local_port_last + 1
            };
            if self.local_port_set.insert(self.local_port_last) {
                break;
            }
        }
        Some(self.local_port_last)
    }

    fn free_local_port(&mut self, port: u32) {
        self.local_port_set.remove(&port);
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

        debug!(
            "vsock: muxer.send[rxq.len={}], (s={}, d={}) op={} len={}",
            self.rxq.len(),
            pkt.hdr.src_port, pkt.hdr.dst_port,
            pkt.hdr.op, pkt.hdr.len
        );

        // TODO: clean up this limit (set a const, etc)
        if self.rxq.len() >= 256 {
            info!("vsock: muxer.rxq full; refusing send()");
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
                    debug!("vsock: new connection to local:{}", pkt.hdr.dst_port);
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
                        "vsock: error establishing unix connection (src={}, dst={}): {:?}",
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

        let mut remove_conn = false;

        self.conn_map.entry(conn_key).and_modify(|conn| {
            if let Err(err) = conn.send_pkt(pkt) {
                remove_conn = true;
                debug!(
                    "vsock: error sending pkt on (src={}, dst={}): {:?}",
                    conn_key.local_port,
                    conn_key.peer_port,
                    err
                );
            }
        });

        if remove_conn {
            self.remove_connection(conn_key);
        }
        else {
            self.rxq.push_back(MuxerRx::ConnRx(conn_key));
        }

        true
    }

    fn recv_pkt(&mut self, max_len: usize) -> Option<VsockPacket> {

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

        // Look for locally issued RSTs, in which case we need to remove the connection.
        //
        if let Some(ref pkt) = maybe_pkt {
            if pkt.hdr.op == uapi::VSOCK_OP_RST {
                self.remove_connection(ConnMapKey {
                    local_port: pkt.hdr.src_port,
                    peer_port: pkt.hdr.dst_port,
                });
            }
            debug!(
                "vsock: muxer.recv[rxq.len={}], (s={}, d={}) op={} len={}",
                self.rxq.len(),
                pkt.hdr.src_port, pkt.hdr.dst_port,
                pkt.hdr.op, pkt.hdr.len
            );
        }

        maybe_pkt
    }

}
