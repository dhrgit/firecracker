// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

use super::*;
use super::{ActivateError, ActivateResult, Queue, VirtioDevice};

use memory_model::{GuestAddress, GuestMemory};
use std::cmp;
use std::result;
use sys_util::EventFd;

use byteorder::{ByteOrder, LittleEndian};
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicUsize, Ordering};

use super::super::Error as DeviceError;
use epoll;
use epoll::Event;
use std::mem;
use std::sync::mpsc;
use std::sync::Arc;
use virtio_gen::virtio_vsock::virtio_vsock_hdr;
use {DeviceEventT, EpollHandler};

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

const fn vsock_hdr_len() -> usize {
    mem::size_of::<virtio_vsock_hdr>()
}

// Frames being sent/received through the network device model have a VNET header. This
// function returns a slice which holds the L2 frame bytes without this header.
fn frame_bytes_from_buf(buf: &[u8]) -> &[u8] {
    &buf[vsock_hdr_len()..]
}

fn frame_bytes_from_buf_mut(buf: &mut [u8]) -> &mut [u8] {
    &mut buf[vsock_hdr_len()..]
}

// This initializes to all 0 the VNET hdr part of a buf.
fn init_vsock_hdr(buf: &mut [u8]) {
    // The buffer should be larger than vnet_hdr_len.
    // TODO: any better way to set all these bytes to 0? Or is this optimized by the compiler?
    for i in 0..vsock_hdr_len() {
        buf[i] = 0;
    }
}

struct TxVirtio {
    queue_evt: EventFd,
    queue: Queue,
}

impl TxVirtio {
    fn new(queue: Queue, queue_evt: EventFd) -> Self {
        let tx_queue_max_size = queue.get_max_size() as usize;
        TxVirtio {
            queue_evt,
            queue,
        }
    }
}

struct RxVirtio {
    queue_evt: EventFd,
    queue: Queue,
}

impl RxVirtio {
    fn new(queue: Queue, queue_evt: EventFd) -> Self {
        RxVirtio {
            queue_evt,
            queue,
        }
    }
}

struct EvVirtio {
    queue_evt: EventFd,
    queue: Queue,
}

impl EvVirtio {
    fn new(queue: Queue, queue_evt: EventFd) -> Self {
        EvVirtio {
            queue_evt,
            queue,
        }
    }
}


struct VsockFrame {
    header: virtio_vsock_hdr,
    data: Vec<u8>,
}

struct VsockEpollHandler {
    rx: RxVirtio,
    tx: TxVirtio,
    ev: EvVirtio,
    mem: GuestMemory,
    interrupt_status: Arc<AtomicUsize>,
    interrupt_evt: EventFd,
    // TODO(smbarber): http://crbug.com/753630
    // Remove once MRG_RXBUF is supported and this variable is actually used.
    #[allow(dead_code)]
    acked_features: u64,
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

    fn process_rx(&mut self) -> result::Result<(), DeviceError> {
        for desc in self.rx.queue.iter(&self.mem) {
            warn!("rx desc: {:?} {}", desc.addr, desc.len);
            let mut buf = vec![0u8; desc.len as usize];

            self.mem.read_slice_at_addr(&mut buf, desc.addr);
            let mut hdr: [u8; vsock_hdr_len()] = [0u8; vsock_hdr_len()];
            hdr.copy_from_slice(&buf[0..vsock_hdr_len()]);

            let hdr =
                unsafe { std::mem::transmute::<[u8; vsock_hdr_len()], virtio_vsock_hdr>(hdr) };

            warn!("hdr: {:?}", hdr);
        }
        Ok(())
    }

    fn resume_rx(&mut self) -> result::Result<(), DeviceError> {
        self.process_rx()
    }

    fn process_tx(&mut self) -> result::Result<(), DeviceError> {
        for desc in self.tx.queue.iter(&self.mem) {
            warn!("tx desc: {:?} {}", desc.addr, desc.len);
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
        warn!("Got event {}", device_event);
        match device_event {
            RX_QUEUE_EVENT => {
                if let Err(e) = self.rx.queue_evt.read() {
                    error!("Failed to get rx queue event: {:?}", e);
                    Err(DeviceError::FailedReadingQueue {
                        event_type: "rx queue event",
                        underlying: e,
                    })
                } else {
                    self.process_rx()
                }
            }
            TX_QUEUE_EVENT => {
                if let Err(e) = self.tx.queue_evt.read() {
                    error!("Failed to get tx queue event: {:?}", e);
                    Err(DeviceError::FailedReadingQueue {
                        event_type: "tx queue event",
                        underlying: e,
                    })
                } else {
                    self.process_tx()
                }
            }
            EVENT_QUEUE_EVENT => {
                warn!("Event queue unimplemented");
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
    pub fn new(cid: u64, epoll_config: EpollConfig) -> Result<Vsock> {
        Ok(Vsock {
            cid,
            avail_features: 0,
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
        warn!("activate vsock");
        if queues.len() != NUM_QUEUES || queue_evts.len() != NUM_QUEUES {
            error!(
                "Cannot perform activate. Expected {} queue(s), got {}",
                NUM_QUEUES,
                queues.len()
            );
            return Err(ActivateError::BadActivate);
        }

        let rx = queues.remove(0);
        let tx = queues.remove(0);
        let ev = queues.remove(0);

        let rx_evt = queue_evts.remove(0);
        let tx_evt = queue_evts.remove(0);
        let ev_evt = queue_evts.remove(0);

        let handler = VsockEpollHandler {
            rx: RxVirtio::new(rx, rx_evt),
            tx: TxVirtio::new(tx, tx_evt),
            ev: EvVirtio::new(ev, ev_evt),
            mem,
            interrupt_status,
            interrupt_evt,
            acked_features: 0,
        };

        let rx_queue_rawfd = handler.rx.queue_evt.as_raw_fd();
        let tx_queue_rawfd = handler.tx.queue_evt.as_raw_fd();
        let ev_queue_rawfd = handler.ev.queue_evt.as_raw_fd();

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
            epoll::Event::new(epoll::Events::EPOLLIN, self.epoll_config.ev_queue_token)
        ).map_err(ActivateError::EpollCtl)?;

        Ok(())
    }
}
