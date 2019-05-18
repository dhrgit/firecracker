// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//


use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::io::{RawFd, AsRawFd};
use std::time::{Instant};

use super::super::defs::uapi;
use super::super::packet::{VsockPacket};
use super::super::{VsockBackend, VsockChannel, VsockEpollListener, VsockError, Result as VsockResult};
use super::connection::VsockConnection;
use super::{Error, Result};


const MAX_CONNECTIONS: usize = 1023;

const RXQ_SIZE: usize = 256;
const KILLQ_SIZE: usize = 128;
const KILLQ_TIMEOUT_MS: u64 = 3000;

const MCF_RXQ: u16 = 1 << 0;
const MCF_KILLQ: u16 = 1 << 1;


#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct ConnMapKey {
    local_port: u32,
    peer_port: u32,
}

#[derive(Debug)]
enum MuxerRx {
    ConnRx(ConnMapKey),
    RstPkt {
        local_port: u32,
        peer_port: u32,
    },
}


enum EpollListener {
    Connection {
        key: ConnMapKey,
        evset: epoll::Events
    },
    HostSock,
    LocalStream(UnixStream),
}

pub struct VsockMuxer {
    cid: u64,
    conn_map: HashMap<ConnMapKey, VsockConnection>,
    listener_map: HashMap<RawFd, EpollListener>,
    rxq: MuxerRxQ,
    killq: MuxerKillQ,
    host_sock: UnixListener,
    host_sock_path: String,
    epoll_fd: RawFd,
    local_port_set: HashSet<u32>,
    local_port_last: u32,
    last_killq_sweep_time: Instant,
}


impl VsockMuxer {

    pub fn new(
        cid: u64,
        host_sock_path: String,
    ) -> Result<Self> {

        let epoll_fd = epoll::create(true)
            .map_err(Error::IoError)?;
        let host_sock = UnixListener::bind(&host_sock_path)
            .and_then(|sock| {
                sock.set_nonblocking(true).map(|_| sock)
            })
            .map_err(Error::IoError)?;

        let mut muxer = Self {
            cid,
            host_sock,
            host_sock_path,
            epoll_fd,
            rxq: MuxerRxQ::new(),
            conn_map: HashMap::with_capacity(MAX_CONNECTIONS),
            listener_map: HashMap::with_capacity(MAX_CONNECTIONS + 1),
            killq: MuxerKillQ::new(),
            local_port_last: (1u32 << 31),
            local_port_set: HashSet::with_capacity(MAX_CONNECTIONS),
            last_killq_sweep_time: Instant::now(),
        };

        muxer.add_listener(muxer.host_sock.as_raw_fd(), EpollListener::HostSock)?;
        Ok(muxer)
    }

    fn process_event(&mut self, fd: RawFd, evset: epoll::Events) {

        debug!(
            "vsock: muxer processing event: fd={}, evset={:?}",
            fd, evset
        );

        if Instant::now().duration_since(self.last_killq_sweep_time).as_secs() > KILLQ_TIMEOUT_MS/1000 {
            self.sweep_killq();
        }

        match self.listener_map.get_mut(&fd) {

            Some(EpollListener::Connection {key, evset}) => {
                let key_copy = *key;
                let evset_copy = *evset;
                self.apply_conn_mutation(key_copy, |conn| {
                    conn.notify(evset_copy);
                });
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
                            warn!("vsock: unable to add listener for local connection: {:?}", err);
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
                                ).unwrap_or_else(|err| {
                                    self.free_local_port(local_port);
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


    fn add_connection(&mut self, key: ConnMapKey, mut conn: VsockConnection) -> Result<()> {

        if self.conn_map.len() >= MAX_CONNECTIONS {
            info!("vsock: muxer connection limit reached ({})", MAX_CONNECTIONS);
            return Err(Error::TooManyConnections);
        }

        self.add_listener(
            conn.get_polled_fd(),
            EpollListener::Connection {key, evset: epoll::Events::EPOLLIN}
        ).and_then(|_| {
            if conn.has_pending_rx() {
                if self.rxq.push(MuxerRx::ConnRx(key)).is_ok() {
                    conn.set_muxer_flag(MCF_RXQ);
                }
            }
            self.conn_map.insert(key, conn);
            Ok(())
        })
    }

    fn remove_connection(&mut self, key: ConnMapKey) {
        if let Some(conn) = self.conn_map.get(&key) {
            self.remove_listener(conn.get_polled_fd());
        }
        self.conn_map.remove(&key);
        self.free_local_port(key.local_port);
    }

    fn remove_connection_with_rst(&mut self, key: ConnMapKey) {
        if !self.rxq.is_full() {
            // There's room left in self.rxq, so it's safe to unwrap here.
            self.rxq.push(MuxerRx::RstPkt {
                local_port: key.local_port,
                peer_port: key.peer_port,
            }).unwrap();
            self.remove_connection(key);
        }
        else {
            self.conn_map.entry(key).and_modify(|conn| {
                conn.kill();
            });
            // This will fail, but we have to do it anyway, so that rxq.synced gets updated.
            self.rxq.push(MuxerRx::ConnRx(key)).unwrap_or_default();
        }
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

    fn handle_peer_request_pkt(&mut self, pkt: &VsockPacket) {

        let port_path = format!("{}_{}", self.host_sock_path, pkt.hdr.dst_port);

        UnixStream::connect(port_path)
            .and_then(|stream| {
                stream.set_nonblocking(true).map(|_| stream)
            })
            .map_err(Error::IoError)
            .and_then(|stream| {
                self.add_connection(
                    ConnMapKey {
                        local_port: pkt.hdr.dst_port,
                        peer_port: pkt.hdr.src_port,
                    },
                    VsockConnection::new_peer_init(
                        stream,
                        self.cid,
                        pkt.hdr.dst_port,
                        pkt.hdr.src_port,
                        pkt.hdr.buf_alloc,
                    )
                )
            })
            .unwrap_or_else(|_| {
                self.rxq.push(MuxerRx::RstPkt {
                    local_port: pkt.hdr.dst_port,
                    peer_port: pkt.hdr.src_port,
                }).unwrap_or_else(|_| {
                    info!("vsock: muxer.rxq full - unable to enqueue RST for peer");
                });
            });
    }

    fn apply_conn_mutation<F>(&mut self, key: ConnMapKey, mut_fn: F)
        where F: FnOnce(&mut VsockConnection)
    {
        if let Some(conn) = self.conn_map.get_mut(&key) {

            mut_fn(conn);

            if conn.has_pending_rx() && !conn.get_muxer_flag(MCF_RXQ) {
                if self.rxq.push(MuxerRx::ConnRx(key)).is_ok() {
                    conn.set_muxer_flag(MCF_RXQ);
                }
            }

            if conn.is_shutting_down() && !conn.get_muxer_flag(MCF_KILLQ) {
                if self.killq.push(key).is_ok() {
                    conn.set_muxer_flag(MCF_KILLQ);
                }
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
                        self.remove_connection_with_rst(key);
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
                    self.remove_connection_with_rst(key);
                });
            }
        }
    }

    fn sweep_killq(&mut self) {

        self.last_killq_sweep_time = Instant::now();

        while let Some(key) = self.killq.pop() {

            debug!(
                "vsock: muxer killing timedout connection (lp={}, pp={})",
                key.local_port, key.peer_port
            );

            self.remove_connection_with_rst(key);
        }

        if !self.killq.is_synced() {
            self.killq.sync(&mut self.conn_map)
                .unwrap_or_default();
        }
    }

}

impl VsockEpollListener for VsockMuxer {

    fn get_polled_fd(&self) -> RawFd {
        self.epoll_fd
    }

    fn get_polled_evset(&self) -> epoll::Events {
        epoll::Events::EPOLLIN
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


}


impl VsockChannel for VsockMuxer {

    fn recv_pkt(&mut self, pkt: &mut VsockPacket) -> VsockResult<()> {

        if !self.rxq.is_synced() {
            self.rxq.sync(&mut self.conn_map).unwrap_or_default();
        }

        while let Some(rx) = self.rxq.pop() {

            let res = match rx {
                MuxerRx::RstPkt{local_port, peer_port} => {
                    pkt.hdr.op = uapi::VSOCK_OP_RST;
                    pkt.hdr.src_cid = uapi::VSOCK_HOST_CID;
                    pkt.hdr.dst_cid = self.cid;
                    pkt.hdr.src_port = local_port;
                    pkt.hdr.dst_port = peer_port;
                    pkt.hdr.len = 0;
                    pkt.hdr.type_ = uapi::VSOCK_TYPE_STREAM;
                    pkt.hdr.flags = 0;
                    pkt.hdr.buf_alloc = 0;
                    pkt.hdr.fwd_cnt = 0;
                    Ok(())
                },
                MuxerRx::ConnRx(key) => {
                    let mut conn_res = Err(VsockError::NoData);
                    self.apply_conn_mutation(key, |conn| {
                        conn.clear_muxer_flag(MCF_RXQ);
                        conn_res = conn.recv_pkt(pkt);
                    });
                    conn_res
                },
            };

            if res.is_ok() {
                if pkt.hdr.op == uapi::VSOCK_OP_RST {
                    self.remove_connection(ConnMapKey {
                        local_port: pkt.hdr.src_port,
                        peer_port: pkt.hdr.dst_port,
                    });
                }

                debug!("vsock muxer: RX pkt: {:?}", *pkt.hdr);
                return Ok(())
            }
        }

        Err(VsockError::NoData)
    }

    fn send_pkt(&mut self, pkt: &VsockPacket) -> VsockResult<()> {

        let conn_key = ConnMapKey {
            local_port: pkt.hdr.dst_port,
            peer_port: pkt.hdr.src_port,
        };

        debug!(
            "vsock: muxer.send[rxq.len={}]: {:?}",
            self.rxq.len(),
            *pkt.hdr
        );

        if pkt.hdr.type_ != uapi::VSOCK_TYPE_STREAM {
            self.rxq.push(MuxerRx::RstPkt {
                local_port: pkt.hdr.dst_port,
                peer_port: pkt.hdr.src_port,
            }).unwrap_or_else(|_| {
                info!("vsock: muxer rxq full - unable to send RST to guest");
            });
            return Ok(());
        }

        if pkt.hdr.dst_cid != uapi::VSOCK_HOST_CID {
            info!("vsock: dropping guest packet for unknown CID: {:?}", *pkt.hdr);
            return Ok(());
        }

        if !self.conn_map.contains_key(&conn_key) {
            if pkt.hdr.op == uapi::VSOCK_OP_REQUEST {
                self.handle_peer_request_pkt(&pkt);
            }
            else {
                self.rxq.push(MuxerRx::RstPkt {
                    local_port: pkt.hdr.dst_port,
                    peer_port: pkt.hdr.src_port,
                }).unwrap_or_else(|_| {
                    info!("vsock: muxer.rxq full - unable to send RST to guest");
                });
            }
            return Ok(());
        }

        if pkt.hdr.op == uapi::VSOCK_OP_RST {
            self.remove_connection(conn_key);
            return Ok(());
        }

        let mut res = Err(VsockError::NoData);
        self.apply_conn_mutation(conn_key, |conn| {
            res = conn.send_pkt(pkt);
        });

        res
    }

}

impl VsockBackend for VsockMuxer {}

struct MuxerRxQ {
    q: VecDeque<MuxerRx>,
    synced: bool,
}

impl MuxerRxQ {
    pub fn new() -> Self {
        Self {
            q: VecDeque::with_capacity(RXQ_SIZE),
            synced: true,
        }
    }
    pub fn push(&mut self, rx: MuxerRx) -> Result<()> {
        if self.q.len() < RXQ_SIZE {
            self.q.push_back(rx);
            return Ok(())
        }
        if let MuxerRx::ConnRx(_) = rx {
            self.synced = false;
        }
        Err(Error::QueueFull)
    }
    pub fn pop(&mut self) -> Option<MuxerRx> {
        self.q.pop_front()
    }
    pub fn sync(&mut self, conn_map: &mut HashMap<ConnMapKey, VsockConnection>) -> Result<()> {
        for (key, conn) in conn_map.iter_mut() {
            if conn.has_pending_rx() && !conn.get_muxer_flag(MCF_RXQ) {
                self.push(MuxerRx::ConnRx(*key))?;
                conn.set_muxer_flag(MCF_RXQ);
            }
        }
        self.synced = true;
        Ok(())
    }
    pub fn is_synced(&self) -> bool {
        self.synced
    }
    pub fn len(&self) -> usize {
        self.q.len()
    }
    pub fn is_full(&self) -> bool {
        self.len() == RXQ_SIZE
    }
}

#[derive(Clone, Copy, Debug)]
struct MuxerKillQItem {
    key: ConnMapKey,
    sched_time: Instant,
}
struct MuxerKillQ {
    q: VecDeque<MuxerKillQItem>,
    synced: bool,
}
impl MuxerKillQ {
    pub fn new() -> Self {
        Self {
            q: VecDeque::with_capacity(KILLQ_SIZE),
            synced: true,
        }
    }
    pub fn push(&mut self, key: ConnMapKey) -> Result<()> {
        if self.q.len() < KILLQ_SIZE {
            self.q.push_back(MuxerKillQItem {
                key,
                sched_time: Instant::now(),
            });
            return Ok(());
        }
        self.synced = false;
        Err(Error::QueueFull)
    }
    pub fn pop(&mut self) -> Option<ConnMapKey> {
        if let Some(item) = self.q.front() {
            let elapsed = item.sched_time.duration_since(Instant::now());
            if elapsed.as_secs()*1000 + u64::from(elapsed.subsec_millis()) >= KILLQ_TIMEOUT_MS {
                return Some(self.q.pop_front().unwrap().key);
            }
        }
        None
    }
    pub fn sync(&mut self, conn_map: &mut HashMap<ConnMapKey, VsockConnection>) -> Result<()> {
        for (key, conn) in conn_map.iter_mut() {
            if conn.is_shutting_down() && !conn.get_muxer_flag(MCF_KILLQ) {
                self.push(*key)?;
            }
        }
        self.synced = true;
        Ok(())
    }
    pub fn is_synced(&self) -> bool {
        self.synced
    }
}
