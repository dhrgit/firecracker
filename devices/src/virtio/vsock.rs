// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

use super::{
    ActivateError, ActivateResult, DescriptorChain, EpollHandlerPayload, Queue, VirtioDevice,
    TYPE_VSOCK, VIRTIO_MMIO_INT_VRING,
};

use memory_model::{DataInit, GuestMemory};
use std::result;
use sys_util::EventFd;

use byteorder::{ByteOrder, LittleEndian};
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicUsize, Ordering};

use super::super::Error as DeviceError;
use epoll;
use std::cmp::min;
use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::mem;
use std::ops::{Deref, DerefMut};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc;
use std::sync::Arc;
use virtio_gen::virtio_vsock::{
    virtio_vsock_hdr, virtio_vsock_op_VIRTIO_VSOCK_OP_CREDIT_UPDATE,
    virtio_vsock_op_VIRTIO_VSOCK_OP_INVALID, virtio_vsock_op_VIRTIO_VSOCK_OP_REQUEST,
    virtio_vsock_op_VIRTIO_VSOCK_OP_RESPONSE, virtio_vsock_op_VIRTIO_VSOCK_OP_RST,
    virtio_vsock_op_VIRTIO_VSOCK_OP_RW, virtio_vsock_op_VIRTIO_VSOCK_OP_SHUTDOWN,
    virtio_vsock_shutdown_VIRTIO_VSOCK_SHUTDOWN_RCV,
    virtio_vsock_shutdown_VIRTIO_VSOCK_SHUTDOWN_SEND, virtio_vsock_type_VIRTIO_VSOCK_TYPE_STREAM,
    VIRTIO_F_VERSION_1,
};
use {DeviceEventT, EpollHandler};
use virtio::vsock::VsockError::GeneralError;
use virtio::vsock::VsockConnectionState::LocalInitiated;

// The guest has made a buffer available to receive a frame into.
const RX_QUEUE_EVENT: DeviceEventT = 0;
// The transmit queue has a frame that is ready to send from the guest.
const TX_QUEUE_EVENT: DeviceEventT = 1;
// rx rate limiter budget is now available.
const EVENT_QUEUE_EVENT: DeviceEventT = 2;
// Number of DeviceEventT events supported by this implementation.
pub const VSOCK_EVENTS_COUNT: usize = 3;

const QUEUE_SIZE: u16 = 256;
const NUM_QUEUES: usize = 3;
const QUEUE_SIZES: &'static [u16] = &[QUEUE_SIZE; NUM_QUEUES];

const MAX_PKT_BUF_SIZE: usize = 65536;

const TEMP_VSOCK_PATH: &str = "./vsock";


#[derive(Debug)]
pub enum VsockError {
    PacketAssemblyError,
    GeneralError,
}
type Result<T> = std::result::Result<T, VsockError>;

// TODO: fix this struct, since packing implines align=1
#[derive(Clone, Copy, Debug, Default)]
#[repr(C, packed)]
struct VsockPacketHdr(virtio_vsock_hdr);

unsafe impl DataInit for VsockPacketHdr {}

impl Deref for VsockPacketHdr {
    type Target = virtio_vsock_hdr;
    fn deref(&self) -> &virtio_vsock_hdr {
        &self.0
    }
}

impl DerefMut for VsockPacketHdr {
    fn deref_mut(&mut self) -> &mut virtio_vsock_hdr {
        &mut self.0
    }
}

// TODO: clean this up. Perhaps discard bindged for vsock altogether
const VSOCK_OP_INVALID: u16 = virtio_vsock_op_VIRTIO_VSOCK_OP_INVALID as u16;
const VSOCK_OP_REQUEST: u16 = virtio_vsock_op_VIRTIO_VSOCK_OP_REQUEST as u16;
const VSOCK_OP_RESPONSE: u16 = virtio_vsock_op_VIRTIO_VSOCK_OP_RESPONSE as u16;
const VSOCK_OP_RST: u16 = virtio_vsock_op_VIRTIO_VSOCK_OP_RST as u16;
const VSOCK_OP_SHUTDOWN: u16 = virtio_vsock_op_VIRTIO_VSOCK_OP_SHUTDOWN as u16;
const VSOCK_OP_RW: u16 = virtio_vsock_op_VIRTIO_VSOCK_OP_RW as u16;
const VSOCK_OP_CREDIT_UPDATE: u16 = virtio_vsock_op_VIRTIO_VSOCK_OP_CREDIT_UPDATE as u16;
const VSOCK_OP_CREDIT_REQUEST: u16 = virtio_vsock_op_VIRTIO_VSOCK_OP_REQUEST as u16;
const VSOCK_FLAGS_SHUTDOWN_RCV: u32 = virtio_vsock_shutdown_VIRTIO_VSOCK_SHUTDOWN_RCV as u32;
const VSOCK_FLAGS_SHUTDOWN_SEND: u32 = virtio_vsock_shutdown_VIRTIO_VSOCK_SHUTDOWN_SEND as u32;
const VSOCK_TYPE_STREAM: u16 = virtio_vsock_type_VIRTIO_VSOCK_TYPE_STREAM as u16;

const VSOCK_HOST_CID: u64 = 2;

#[derive(Default)]
struct VsockPacket {
    hdr: VsockPacketHdr,
    buf: Vec<u8>,
}

impl VsockPacket {
    fn new_rst(src_cid: u64, dst_cid: u64, src_port: u32, dst_port: u32) -> Self {
        Self {
            hdr: VsockPacketHdr(virtio_vsock_hdr {
                src_cid,
                dst_cid,
                src_port,
                dst_port,
                type_: VSOCK_TYPE_STREAM,
                op: VSOCK_OP_RST,
                ..Default::default()
            }),
            buf: Vec::new(),
        }
    }
    fn new_response(src_cid: u64, dst_cid: u64, src_port: u32, dst_port: u32) -> Self {
        Self {
            hdr: VsockPacketHdr(virtio_vsock_hdr {
                src_cid,
                dst_cid,
                src_port,
                dst_port,
                type_: VSOCK_TYPE_STREAM,
                op: VSOCK_OP_RST,
                ..Default::default()
            }),
            buf: Vec::new(),
        }
    }
    fn new_rw(src_cid: u64, dst_cid: u64, src_port: u32, dst_port: u32, buf: Vec<u8>) -> Self {
        Self {
            hdr: VsockPacketHdr(virtio_vsock_hdr {
                src_cid,
                dst_cid,
                src_port,
                dst_port,
                type_: VSOCK_TYPE_STREAM,
                op: VSOCK_OP_RST,
                ..Default::default()
            }),
            buf,
        }
    }

}

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



    pub fn send(&mut self, pkt: VsockPacket) {
        match pkt.hdr.op {
            VSOCK_OP_RW => {
                self.remote.write(pkt.buf.as_slice());
            },
            _ => (),
        }
    }

    pub fn recv(&mut self, max_len: usize) -> Result<VsockPacket> {
        Err(VsockError::GeneralError)
    }
}

struct VsockMuxer {
    rxq: VecDeque<VsockPacket>,
    conn_map: HashMap<(u32, u32), VsockConnection>,
}

impl VsockMuxer {
    pub fn new() -> Self {
        Self {
            rxq: VecDeque::new(),
            conn_map: HashMap::new(),
        }
    }

    pub fn send(&mut self, pkt: VsockPacket) -> Result<()> {
        let conn_key = (pkt.hdr.src_port, pkt.hdr.dst_port);

        // TODO: validate pkt. e.g. type = stream, cid, etc
        //

        if !self.conn_map.contains_key(&conn_key) {
            if pkt.hdr.op != VSOCK_OP_REQUEST {
                warn!("vsock: dropping unexpected packet from guest, op={}", pkt.hdr.op);
                return Err(GeneralError);
            }

            self.conn_map.insert(
                conn_key,
                VsockConnection::new_local_init(pkt.hdr.src_port, pkt.hdr.dst_port)
            );

            return Ok(());
        }

        self.conn_map.entry(conn_key).and_modify(|conn| {
            conn.send(pkt);
        });

        Ok(())

    }

    pub fn recv(&mut self, max_len: usize) -> Option<VsockPacket> {
        self.rxq.pop_front()
    }
}

struct VsockEpollHandler {
    rxvq: Queue,
    rxvq_evt: EventFd,
    txvq: Queue,
    txvq_evt: EventFd,
    evq: Queue,
    evq_evt: EventFd,
    cid: u64,
    mem: GuestMemory,
    interrupt_status: Arc<AtomicUsize>,
    interrupt_evt: EventFd,
    muxer: VsockMuxer,
}

impl VsockEpollHandler {
    fn signal_used_queue(&self) -> result::Result<(), DeviceError> {
        self.interrupt_status
            .fetch_or(VIRTIO_MMIO_INT_VRING as usize, Ordering::SeqCst);
        self.interrupt_evt.write(1).map_err(|e| {
            error!("Failed to signal used queue: {:?}", e);
            DeviceError::FailedSignalingUsedQueue(e)
        })
    }

    fn process_rx(&mut self) {
        debug!("vsock RX q event");
        let mut raise_irq = false;
        while let Some(head) = self.rxvq.iter(&self.mem).next() {
            let mut max_len = 0usize;

            let mut maybe_desc = head.next_descriptor();
            while let Some(desc) = maybe_desc {
                max_len += desc.len as usize;
                maybe_desc = desc.next_descriptor();
            }

            if let Some(pkt) = self.muxer.recv(max_len) {
                debug!("vsock rx: writing pkt: {:#?}", pkt.hdr);
                let len = pkt.hdr.len + (mem::size_of::<VsockPacketHdr>() as u32);
                match self.write_pkt_to_desc_chain(pkt, &head) {
                    Err(e) => {
                        warn!("vsock: error writing pkt to guest mem: {:?}", e);
                        self.rxvq.go_to_previous_position();
                        break;
                    }
                    Ok(_) => raise_irq = true,
                }
                self.rxvq.add_used(&self.mem, head.index, len);
            } else {
                break;
            }
        }
        if raise_irq {
            match self.signal_used_queue() {
                Err(e) => warn!("vsock: failed to trigger IRQ: {:?}", e),
                Ok(_) => (),
            }
        }
    }

    fn process_tx(&mut self) {
        debug!("vsock TX q event");
        while let Some(head) = self.txvq.iter(&self.mem).next() {
            let pkt = match self.read_pkt_from_desc_chain(&head) {
                Ok(pkt) => {
                    debug!("vsock: got TX packet: {:#?}", pkt.hdr);
                    pkt
                }
                Err(e) => {
                    // Reading from the TX queue shouldn't fail. If it does, though, we'll
                    // just drop the packet. It's fine, the vsock driver does it as well.
                    debug!("vsock: error reading TX packet: {:?}", e);
                    continue;
                }
            };
            // TODO: handle muxer send error here. What do we do with the in-flight packet?
            self.muxer.send(pkt).unwrap();
            self.txvq.add_used(&self.mem, head.index, 0);
        }

        self.process_rx();
    }

    fn read_pkt_from_desc_chain(&self, head: &DescriptorChain) -> Result<VsockPacket> {
        if (head.len as usize) < std::mem::size_of::<VsockPacketHdr>() {
            warn!("vsock: framing error, TX desc head too small for packet header");
            return Err(VsockError::PacketAssemblyError);
        }

        // TODO: check descriptor is readable

        let hdr = self
            .mem
            .read_obj_from_addr::<VsockPacketHdr>(head.addr)
            .map_err(|e| VsockError::PacketAssemblyError)?;

        // TODO: check hdr.len holds a sane value
        //let mut buf = Vec::with_capacity(hdr.len as usize);
        let mut buf = vec![0u8; hdr.len as usize];
        buf.resize(hdr.len as usize, 0);

        let mut read_cnt = 0usize;
        let mut maybe_desc = head.next_descriptor();
        while let Some(desc) = maybe_desc {
            // TODO: check descriptor is readable
            if read_cnt + (desc.len as usize) > (hdr.len as usize) {
                warn!("vsock: malformed TX packet, vring data > hdr.len");
                return Err(VsockError::PacketAssemblyError);
            }
            self.mem
                .read_slice_at_addr(&mut buf[read_cnt..read_cnt + desc.len as usize], desc.addr)
                .map_err(|e| VsockError::PacketAssemblyError)?;
            read_cnt += desc.len as usize;
            maybe_desc = desc.next_descriptor();
        }

        if read_cnt != (hdr.len as usize) {
            warn!("vsock: malformed TX packet, vring data != hdr.len");
            return Err(VsockError::PacketAssemblyError);
        }

        Ok(VsockPacket { hdr, buf })
    }

    fn write_pkt_to_desc_chain(&self, pkt: VsockPacket, head: &DescriptorChain) -> Result<()> {
        if (head.len as usize) < mem::size_of::<VsockPacketHdr>() {
            return Err(VsockError::GeneralError);
        }

        if !head.is_write_only() {
            return Err(VsockError::GeneralError);
        }

        self.mem
            .write_obj_at_addr::<VsockPacketHdr>(pkt.hdr, head.addr)
            .map_err(|_| VsockError::GeneralError)?;

        let mut write_cnt = 0usize;
        let mut maybe_desc = head.next_descriptor();
        while let Some(desc) = maybe_desc {
            if !desc.is_write_only() {
                return Err(VsockError::GeneralError);
            }
            let write_end = min(pkt.buf.len(), write_cnt + desc.len as usize);
            write_cnt += self
                .mem
                .write_slice_at_addr(&pkt.buf[write_cnt..write_end], desc.addr)
                .map_err(|_| VsockError::GeneralError)?;
            maybe_desc = desc.next_descriptor();
        }

        Ok(())
    }
}

impl EpollHandler for VsockEpollHandler {
    fn handle_event(
        &mut self,
        device_event: DeviceEventT,
        _: u32,
        payload: EpollHandlerPayload,
    ) -> result::Result<(), DeviceError> {
        match device_event {
            RX_QUEUE_EVENT => {
                if let Err(e) = self.rxvq_evt.read() {
                    error!("Failed to get rx queue event: {:?}", e);
                    Err(DeviceError::FailedReadingQueue {
                        event_type: "rx queue event",
                        underlying: e,
                    })
                } else {
                    self.process_rx();
                    Ok(())
                }
            }
            TX_QUEUE_EVENT => {
                if let Err(e) = self.txvq_evt.read() {
                    error!("Failed to get tx queue event: {:?}", e);
                    Err(DeviceError::FailedReadingQueue {
                        event_type: "tx queue event",
                        underlying: e,
                    })
                } else {
                    self.process_tx();
                    Ok(())
                }
            }
            EVENT_QUEUE_EVENT => {
                warn!("Event queue unimplemented");
                if let Err(e) = self.evq_evt.read() {
                    error!("Failed to consume evq event: {:?}", e);
                    return Err(DeviceError::FailedReadingQueue {
                        event_type: "ev queue event",
                        underlying: e,
                    });
                }
                Ok(())
            }
            other => Err(DeviceError::UnknownEvent {
                device: "vsock",
                event: other,
            }),
        }
    }
}

pub struct EpollConfig {
    rx_queue_token: u64,
    tx_queue_token: u64,
    ev_queue_token: u64,
    epoll_raw_fd: RawFd,
    sender: mpsc::Sender<Box<EpollHandler>>,
}

impl EpollConfig {
    pub fn new(
        first_token: u64,
        epoll_raw_fd: RawFd,
        sender: mpsc::Sender<Box<EpollHandler>>,
    ) -> Self {
        EpollConfig {
            rx_queue_token: first_token + RX_QUEUE_EVENT as u64,
            tx_queue_token: first_token + TX_QUEUE_EVENT as u64,
            ev_queue_token: first_token + EVENT_QUEUE_EVENT as u64,
            epoll_raw_fd,
            sender,
        }
    }
}

pub struct Vsock {
    cid: u64,
    avail_features: u64,
    acked_features: u64,
    config_space: Vec<u8>,
    epoll_config: EpollConfig,
}

impl Vsock {
    /// Create a new virtio-vsock device with the given VM cid.
    pub fn new(cid: u64, epoll_config: EpollConfig) -> super::Result<Vsock> {
        Ok(Vsock {
            cid,
            //            avail_features: 1u64 << VIRTIO_F_ANY_LAYOUT | 1u64 << VIRTIO_F_VERSION_1,
            avail_features: 1u64 << VIRTIO_F_VERSION_1,
            acked_features: 0,
            config_space: Vec::new(),
            epoll_config,
        })
    }
}

impl VirtioDevice for Vsock {
    fn device_type(&self) -> u32 {
        TYPE_VSOCK
    }

    fn queue_max_sizes(&self) -> &[u16] {
        QUEUE_SIZES
    }

    fn features(&self, page: u32) -> u32 {
        match page {
            // Get the lower 32-bits of the features bitfield.
            0 => self.avail_features as u32,
            // Get the upper 32-bits of the features bitfield.
            1 => (self.avail_features >> 32) as u32,
            _ => {
                warn!(
                    "vsock: virtio-vsock got request for features page: {}",
                    page
                );
                0u32
            }
        }
    }

    fn ack_features(&mut self, page: u32, value: u32) {
        let mut v = match page {
            0 => value as u64,
            1 => (value as u64) << 32,
            _ => {
                warn!(
                    "vsock: virtio-vsock device cannot ack unknown feature page: {}",
                    page
                );
                0u64
            }
        };

        // Check if the guest is ACK'ing a feature that we didn't claim to have.
        let unrequested_features = v & !self.avail_features;
        if unrequested_features != 0 {
            warn!("vsock: virtio-vsock got unknown feature ack: {:x}", v);

            // Don't count these features as acked.
            v &= !unrequested_features;
        }
        self.acked_features |= v;
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        match offset {
            0 if data.len() == 8 => LittleEndian::write_u64(data, self.cid),
            0 if data.len() == 4 => LittleEndian::write_u32(data, (self.cid & 0xffffffff) as u32),
            4 if data.len() == 4 => {
                LittleEndian::write_u32(data, ((self.cid >> 32) & 0xffffffff) as u32)
            }
            _ => warn!(
                "vsock: virtio-vsock received invalid read request of {} bytes at offset {}",
                data.len(),
                offset
            ),
        }
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        let data_len = data.len() as u64;
        let config_len = self.config_space.len() as u64;
        if offset + data_len > config_len {
            error!("Failed to write config space");
            return;
        }
        let (_, right) = self.config_space.split_at_mut(offset as usize);
        right.copy_from_slice(&data[..]);
    }

    fn activate(
        &mut self,
        mem: GuestMemory,
        interrupt_evt: EventFd,
        interrupt_status: Arc<AtomicUsize>,
        mut queues: Vec<Queue>,
        mut queue_evts: Vec<EventFd>,
    ) -> ActivateResult {
        if queues.len() != NUM_QUEUES || queue_evts.len() != NUM_QUEUES {
            error!(
                "Cannot perform activate. Expected {} queue(s), got {}",
                NUM_QUEUES,
                queues.len()
            );
            return Err(ActivateError::BadActivate);
        }

        let rxvq = queues.remove(0);
        let txvq = queues.remove(0);
        let evq = queues.remove(0);

        let rxvq_evt = queue_evts.remove(0);
        let txvq_evt = queue_evts.remove(0);
        let evq_evt = queue_evts.remove(0);

        let handler = VsockEpollHandler {
            rxvq,
            rxvq_evt,
            txvq,
            txvq_evt,
            evq,
            evq_evt,
            mem,
            cid: self.cid,
            interrupt_status,
            interrupt_evt,
            muxer: VsockMuxer::new(),
        };

        let rx_queue_rawfd = handler.rxvq_evt.as_raw_fd();
        let tx_queue_rawfd = handler.txvq_evt.as_raw_fd();
        let ev_queue_rawfd = handler.evq_evt.as_raw_fd();

        self.epoll_config
            .sender
            .send(Box::new(handler))
            .expect("Failed to send handler through channel");

        epoll::ctl(
            self.epoll_config.epoll_raw_fd,
            epoll::ControlOptions::EPOLL_CTL_ADD,
            rx_queue_rawfd,
            epoll::Event::new(epoll::Events::EPOLLIN, self.epoll_config.rx_queue_token),
        )
        .map_err(ActivateError::EpollCtl)?;

        epoll::ctl(
            self.epoll_config.epoll_raw_fd,
            epoll::ControlOptions::EPOLL_CTL_ADD,
            tx_queue_rawfd,
            epoll::Event::new(epoll::Events::EPOLLIN, self.epoll_config.tx_queue_token),
        )
        .map_err(ActivateError::EpollCtl)?;

        epoll::ctl(
            self.epoll_config.epoll_raw_fd,
            epoll::ControlOptions::EPOLL_CTL_ADD,
            ev_queue_rawfd,
            epoll::Event::new(epoll::Events::EPOLLIN, self.epoll_config.ev_queue_token),
        )
        .map_err(ActivateError::EpollCtl)?;

        Ok(())
    }
}
