// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//


use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::io::{RawFd, AsRawFd};

use super::super::defs::uapi;
use super::super::packet::{VsockPacket, VsockPacketHdr};
use super::super::VsockBackend;
use super::connection::VsockConnection;
use super::{Error, Result, VSOCK_TX_BUF_SIZE};


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
    LocalStream(UnixStream),
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

        debug!(
            "vsock: muxer processing event: fd={}, evset={:?}",
            fd, evset
        );
        match self.listener_map.get_mut(&fd) {

            Some(EpollListener::Connection {key, evset}) => {
                if let Some(conn) = self.conn_map.get_mut(key) {
                    conn.notify(*evset);
                    let key_copy = *key;
                    self.conn_mutation_hook(key_copy);
                }
            },

            Some(EpollListener::HostSock) => {
                match self.host_sock.accept() {
                    Ok((stream, name)) => {
                        match stream.set_nonblocking(true) {
                            Err(err) => {
                                warn!(
                                    "vsock: unable to set local connection non-blocking: {:?}",
                                    err
                                );
                            },
                            Ok(()) => ()
                        }
                        debug!("vsock: new local connection: {:?}", name);
                        self.add_listener(
                            stream.as_raw_fd(),
                            EpollListener::LocalStream(stream)
                        ).unwrap_or_else(|err| {
                            warn!(
                                "vsock: unable to add listener for local connection: {:?}",
                                err
                            );
                        });
                    },
                    Err(err) => {
                        warn!("vsock: error accepting local connection: {:?}", err);
                    }
                }
            },

            Some(EpollListener::LocalStream(_)) => {
                if let Some(EpollListener::LocalStream(mut stream)) = self.remove_listener(fd) {
                    match Self::read_local_stream_port(&mut stream) {
                        Err(err) => {
                            error!("vsock: error reading port from local stream: {:?}", err);
                        },
                        Ok(peer_port) => {
                            if let Some(local_port) = self.allocate_local_port() {
                                let conn_key = ConnMapKey {local_port, peer_port};
                                self.add_connection(
                                    conn_key,
                                    VsockConnection::new_local_init(
                                        stream,
                                        self.cid,
                                        local_port,
                                        peer_port,
                                    )
                                ).and_then(|_| {
                                    self.rxq.push_back(MuxerRx::ConnRx(conn_key));
                                    Ok(())
                                }).unwrap_or_else(|err| {
                                    error!("vsock: error adding connection: {:?}", err);
                                });
                            }
                        }
                    }
                }
            }

            _ => {
                warn!("vsock: unexpected event: fd={:?}, evset={:?}", fd, evset);
            },
        }
    }

    fn read_local_stream_port(stream: &mut UnixStream) -> Result<u32> {
        // TODO: define this port negociation protocol
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
                        error!("vsock: invalid data read from local stream: {:?}", err);
                        Err(Error::ProtocolError)
                    }
                }
            }
        }
    }

    fn conn_mutation_hook(&mut self, key: ConnMapKey) {
        if let Some(conn) = self.conn_map.get_mut(&key) {
            if conn.has_pending_rx() {
                self.rxq.push_back(MuxerRx::ConnRx(key));
            }
            let fd = conn.get_polled_fd();
            let new_evset = conn.get_polled_evset();
            if new_evset.is_empty() {
                self.remove_listener(fd);
                return;
            }
            if let Some(EpollListener::Connection {evset,..}) = self.listener_map.get_mut(&fd) {
                if *evset != new_evset {

                    debug!(
                        "vsock: updating listener for (lp={}, pp={}): old={:?}, new={:?}",
                        key.local_port, key.peer_port, *evset, new_evset
                    );

                    *evset = new_evset;
                    epoll::ctl(
                        self.epoll_fd,
                        epoll::ControlOptions::EPOLL_CTL_MOD,
                        fd,
                        epoll::Event::new(new_evset, fd as u64),
                    ).unwrap_or_else(|err| {
                        warn!(
                            "vsock: error updating epoll listener for (lp={}, pp={}): {:?}",
                            key.local_port, key.peer_port, err
                        );
                        self.remove_connection(key);
                    });
                }
            }
            else {
                self.add_listener(
                    fd,
                    EpollListener::Connection {key, evset: new_evset}
                ).unwrap_or_else(|err| {
                    warn!(
                        "vsock: error adding epoll listener for (lp={}, pp={}): {:?}",
                        key.local_port, key.peer_port, err
                    );
                    self.remove_connection(key);
                });
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
            debug!("vsock: error adding listener: {:?}", err);
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
            EpollListener::LocalStream(_) => epoll::Events::EPOLLIN,
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

        let maybe_listener = self.listener_map.remove(&fd);

        if maybe_listener.is_some() {
            epoll::ctl(
                self.epoll_fd,
                epoll::ControlOptions::EPOLL_CTL_DEL,
                fd,
                epoll::Event::new(epoll::Events::empty(), 0)
            ).unwrap_or_else(|err| {
                warn!("vosck muxer: error removing epoll listener for fd {:?}: {:?}", fd, err);
            });
        }

        maybe_listener
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

    fn new_local_pkt(&self, op: u16, src_port: u32, dst_port: u32) -> VsockPacket {
        VsockPacket {
            hdr: VsockPacketHdr {
                src_cid: uapi::VSOCK_HOST_CID,
                dst_cid: self.cid,
                src_port,
                dst_port,
                len: 0,
                type_: uapi::VSOCK_TYPE_STREAM,
                op,
                flags: 0,
                buf_alloc: VSOCK_TX_BUF_SIZE as u32,
                fwd_cnt: 0,
                .. Default::default()
            },
            buf: Vec::new(),
        }
    }

    fn enq_rst(&mut self, src_port: u32, dst_port: u32) {
        self.rxq.push_back(MuxerRx::Packet(
            self.new_local_pkt(uapi::VSOCK_OP_RST, src_port, dst_port)
        ));
    }


}

impl VsockBackend for VsockMuxer {

    fn get_epoll_listener(&self) -> (RawFd, epoll::Events) {
        (self.epoll_fd, epoll::Events::EPOLLIN)
    }

    fn notify(&mut self, _: epoll::Events) {

        debug!("vsock: muxer received kick");

        let mut epoll_events = vec![epoll::Event::new(epoll::Events::empty(), 0); 16];
        match epoll::wait(self.epoll_fd, 0, epoll_events.as_mut_slice()) {
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
    // true == packet consumed
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
                info!("vsock: dropping unexpected packet from guest: {:?}", pkt.hdr);
                return true;
            }

            match self.handle_peer_request_pkt(&pkt) {
                Ok(()) => self.rxq.push_back(MuxerRx::ConnRx(conn_key)),
                Err(err) => {
                    self.enq_rst(pkt.hdr.dst_port, pkt.hdr.src_port);
                    info!(
                        "vsock: error accepting connection request from guest (lp={}, pp={}): {:?}",
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

        if let Some(conn) = self.conn_map.get_mut(&conn_key) {
            conn.send_pkt(pkt);
            self.conn_mutation_hook(conn_key);
        }

        true
    }

    fn recv_pkt(&mut self, max_len: usize) -> Option<VsockPacket> {

        // TODO: if conn.recv_pkt returns None, try the next item in self.rxq
        let maybe_pkt: Option<VsockPacket> = match self.rxq.pop_front() {
            Some(MuxerRx::Packet(pkt)) => {
                Some(pkt)
            },
            Some(MuxerRx::ConnRx(key)) => {
                if let Some(conn) = self.conn_map.get_mut(&key) {
                    let pkt = conn.recv_pkt(max_len);
                    self.conn_mutation_hook(key);
                    pkt
                }
                else {
                    None
                }
            },
            None => None
        };


        // TODO: figure out when to unregister connections from EPOLLIN stream events
        // Also, host-incoming connections might need to get dropped if the dirver locked the RX q

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
