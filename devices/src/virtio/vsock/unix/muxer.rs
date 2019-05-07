// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//


use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::io::{RawFd, AsRawFd};

use super::super::defs::uapi;
use super::super::packet::{VsockPacket};
use super::super::VsockBackend;
use super::connection::VsockConnection;
use super::{Error, Result};


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
    HostSock,
    FutureConn(UnixStream),
}

pub struct VsockMuxer {
    rxq: VecDeque<MuxerRx>,
    conn_map: HashMap<ConnMapKey, VsockConnection>,
    listener_map: HashMap<RawFd, EpollListener>,
    cid: u64,
    host_sock: UnixListener,
    host_sock_path: String,
    epoll_fd: RawFd,
    local_port_set: HashSet<u32>,
    local_port_last: u32,
}

impl VsockMuxer {

    pub fn new(
        cid: u64,
        host_sock: UnixListener,
        host_sock_path: String,
        epoll_fd: RawFd
    ) -> Result<Self> {

        let host_sock_fd = host_sock.as_raw_fd();

        let mut muxer = Self {
            rxq: VecDeque::new(),
            conn_map: HashMap::new(),
            listener_map: HashMap::new(),
            cid,
            host_sock,
            host_sock_path,
            epoll_fd,
            local_port_last: (1u32 << 31),
            local_port_set: HashSet::new(),
        };

        muxer.add_listener(host_sock_fd, EpollListener::HostSock)?;
        Ok(muxer)
    }


    fn process_event(&mut self, fd: RawFd, evset: epoll::Events) {

        match self.listener_map.get_mut(&fd) {

            Some(EpollListener::Connection {key, evset: conn_evset}) => {
                let conn_key_copy = *key;
                let conn_evset_copy = *conn_evset;
                match self.process_conn_event(conn_key_copy, conn_evset_copy, fd, evset) {
                    Ok(()) => (),
                    Err(err) => {
                        error!(
                            "vsock muxer: error processing connection event: {:?}",
                            err
                        );
                        self.remove_connection(conn_key_copy);
                        // TODO: try send rst
                    }
                }
            },

            Some(EpollListener::HostSock) => {
                match self.host_sock.accept() {
                    Ok((stream, name)) => {
                        match stream.set_nonblocking(true) {
                            Err(err) => {
                                error!(
                                    "vsock muxer: unable to set local connection non-blocking: {:?}",
                                    err
                                );
                            },
                            Ok(()) => ()
                        }
                        debug!("vsock muxer: new local connection: {:?}", name);
                        self.add_listener(
                            stream.as_raw_fd(),
                            EpollListener::FutureConn(stream)
                        ).unwrap_or_else(|err| {
                            error!(
                                "vsock muxer: unable to add listener for local connection: {:?}",
                                err
                            );
                        });
                    },
                    Err(err) => {
                        error!("vsock muxer: error accepting local connection: {:?}", err);
                    }
                }
            },

            Some(EpollListener::FutureConn(_)) => {
                if let Some(EpollListener::FutureConn(mut stream)) = self.remove_listener(fd) {
                    match Self::read_local_stream_port(&mut stream) {
                        Err(err) => {
                            error!("vsock muxer: error reading port from local stream: {:?}", err);
                        },
                        Ok(peer_port) => {
                            if let Some(local_port) = self.allocate_local_port() {
                                let conn_key = ConnMapKey {local_port, peer_port};
                                self.add_connection(
                                    conn_key,
                                    VsockConnection::new_local_init(
                                        self.cid,
                                        local_port,
                                        peer_port,
                                        stream
                                    )
                                ).and_then(|_| {
                                    self.rxq.push_back(MuxerRx::ConnRx(conn_key));
                                    Ok(())
                                }).unwrap_or_else(|err| {
                                    error!("vsock muxer: error adding connection: {:?}", err);
                                });
                            }
                        }
                    }
                }
            }

            _ => {
                warn!("vsock muxer: unexpected event: fd={:?}, evset={:?}", fd, evset);
            },
        }
    }

    fn process_conn_event(
        &mut self,
        key: ConnMapKey,
        conn_evset: epoll::Events,
        fd: RawFd,
        evset: epoll::Events
    ) -> Result<()> {

        let mut new_evset = conn_evset;

        if evset.contains(epoll::Events::EPOLLOUT) {
            let mut flush_res = Ok(true);
            self.conn_map.entry(key).and_modify(|conn| {
                flush_res = conn.flush_tx_buf();
            });
            match flush_res {
                Err(err) => return Err(err),
                Ok(true) => {
                    new_evset.remove(epoll::Events::EPOLLOUT);
                },
                Ok(false) => ()
            }
        }

        if evset.contains(epoll::Events::EPOLLIN) {
            self.rxq.push_back(MuxerRx::ConnRx(key));
        }

        if new_evset != conn_evset {
            self.modify_listener_evset(fd, new_evset)?;
        }

        Ok(())
    }

    fn read_local_stream_port(stream: &mut UnixStream) -> Result<u32> {
        // TODO: Check input more thoroughly, define it better
        let mut buf = [0u8; 32];
        match stream.read(&mut buf) {
            Ok(0) => Err(Error::BrokenPipe),
            Err(e) => Err(Error::IoError(e)),
            Ok(read_cnt) => {
                match std::str::from_utf8(buf.chunks(read_cnt).next().unwrap()) {
                    Ok(s) => {
                        s.trim().parse::<u32>().map_err(|_| Error::ProtocolError)
                    }
                    Err(err) => {
                        error!("vsock muxer: invalid data read from local stream: {:?}", err);
                        Err(Error::ProtocolError)
                    }
                }
            }
        }
    }


    fn add_connection(&mut self, key: ConnMapKey, conn: VsockConnection) -> Result<()> {

        self.add_listener(
            conn.as_raw_fd(),
            EpollListener::Connection {key, evset: epoll::Events::EPOLLIN}
        ).and_then(|_| {
            self.conn_map.insert(key, conn);
            Ok(())
        }).map_err(|err| {
            debug!("vsock muxer: error adding listener: {:?}", err);
            err
        })
    }

    fn remove_connection(&mut self, key: ConnMapKey) {
        if let Some(conn) = self.conn_map.get(&key) {
            self.remove_listener(conn.as_raw_fd());
        }
        self.conn_map.remove(&key);
        self.free_local_port(key.local_port);
    }

    fn add_listener(&mut self, fd: RawFd, listener: EpollListener) -> Result<()> {

        let evset = match listener {
            EpollListener::Connection {evset, ..} => evset,
            EpollListener::FutureConn(_) => epoll::Events::EPOLLIN,
            EpollListener::HostSock => epoll::Events::EPOLLIN,
        };

        epoll::ctl(
            self.epoll_fd,
            epoll::ControlOptions::EPOLL_CTL_ADD,
            fd,
            epoll::Event::new(evset, fd as u64),
        ).and_then(|_| {
            self.listener_map.insert(fd, listener);
            Ok(())
        }).map_err(Error::IoError)?;

        Ok(())
    }

    fn remove_listener(&mut self, fd: RawFd) -> Option<EpollListener> {

        epoll::ctl(
           self.epoll_fd,
            epoll::ControlOptions::EPOLL_CTL_DEL,
            fd,
            epoll::Event::new(epoll::Events::empty(), 0)
        ).unwrap_or_else(|err| {
            error!("vosck muxer: error removing epoll listener for fd {:?}: {:?}", fd, err);
        });

        self.listener_map.remove(&fd)
    }

    fn modify_listener_evset(&mut self, fd: RawFd, evset: epoll::Events) -> Result<()> {
        epoll::ctl(
            self.epoll_fd,
            epoll::ControlOptions::EPOLL_CTL_MOD,
            fd,
            epoll::Event::new(evset, fd as u64)
        ).map_err(Error::IoError)
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

    fn handle_peer_request_pkt(&mut self, pkt: &VsockPacket) -> Result<()> {

        let port_path = format!("{}_{}", self.host_sock_path, pkt.hdr.dst_port);
        let stream = UnixStream::connect(port_path)
            .and_then(|stream| {
                stream.set_nonblocking(true).map(|_| stream)
            })
            .map_err(Error::IoError)?;

        let conn_key = ConnMapKey {
            local_port: pkt.hdr.dst_port,
            peer_port: pkt.hdr.src_port,
        };
        let conn = VsockConnection::new_peer_init(
            stream,
            self.cid,
            pkt.hdr.dst_port,
            pkt.hdr.src_port,
            pkt.hdr.buf_alloc,
        );
        self.add_connection(conn_key, conn)
            .and_then(|_| {
                self.rxq.push_back(MuxerRx::ConnRx(conn_key));
                Ok(())
            })?;

        Ok(())
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
                info!("vsock muxer: dropping unexpected packet from guest: {:?}", pkt.hdr);
                return true;
            }

            match self.handle_peer_request_pkt(&pkt) {
                Ok(()) => self.rxq.push_back(MuxerRx::ConnRx(conn_key)),
                Err(err) => {
                    self.rxq.push_back(MuxerRx::Packet(VsockPacket::new_rst(
                        self.cid,
                        pkt.hdr.src_cid,
                        pkt.hdr.dst_port,
                        pkt.hdr.src_port
                    )));
                    info!(
                        "vsock muxer: error accepting connection request from guest (lp={}, pp={}): {:?}",
                        conn_key.local_port, conn_key.peer_port, err
                    );
                }
            }

            return true;
        }

        if pkt.hdr.op == uapi::VSOCK_OP_RST {
            self.remove_connection(conn_key);
            return true;
        }

        let mut send_result = Ok(true);
        let mut conn_fd: RawFd = 0;
        self.conn_map.entry(conn_key).and_modify(|conn| {
            send_result = conn.send_pkt(pkt);
            conn_fd = conn.as_raw_fd();
        });
        match send_result {
            Ok(drained) => {
                if let Some(EpollListener::Connection {evset, ..}) = self.listener_map.get_mut(&conn_fd) {
                    if drained == evset.contains(epoll::Events::EPOLLOUT) {
                        if drained {
                            evset.remove(epoll::Events::EPOLLOUT);
                        }
                        else {
                            evset.insert(epoll::Events::EPOLLOUT);
                        }
                        let _evs = *evset;
                        self.modify_listener_evset(conn_fd, _evs)
                            .unwrap_or_else(|err| {
                                error!(
                                    "vsock muxer: error modifying epoll listener for connection (lp={}, pp={}): {:?}",
                                    conn_key.local_port, conn_key.peer_port, err
                                );
                                self.remove_connection(conn_key);
                            });
                    }
                }
            }
            Err(err) => {
                self.remove_connection(conn_key);
                error!(
                    "vsock muxer: error sending data on connection (lp={}, pp={}): {:?}",
                    conn_key.local_port, conn_key.peer_port, err
                );
            }
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
