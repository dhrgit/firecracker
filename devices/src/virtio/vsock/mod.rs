// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

mod connection;
mod epoll_handler;
mod muxer;
mod packet;



use std::result;
use std::sync::{Arc, mpsc};
use std::sync::atomic::AtomicUsize;
use std::os::unix::io::{AsRawFd, RawFd};

use byteorder::{ByteOrder, LittleEndian};

use memory_model::GuestMemory;
use sys_util::EventFd;
use virtio_gen::virtio_vsock::{
    virtio_vsock_op_VIRTIO_VSOCK_OP_CREDIT_UPDATE,
    virtio_vsock_op_VIRTIO_VSOCK_OP_INVALID, virtio_vsock_op_VIRTIO_VSOCK_OP_REQUEST,
    virtio_vsock_op_VIRTIO_VSOCK_OP_RESPONSE, virtio_vsock_op_VIRTIO_VSOCK_OP_RST,
    virtio_vsock_op_VIRTIO_VSOCK_OP_RW, virtio_vsock_op_VIRTIO_VSOCK_OP_SHUTDOWN,
    virtio_vsock_shutdown_VIRTIO_VSOCK_SHUTDOWN_RCV,
    virtio_vsock_shutdown_VIRTIO_VSOCK_SHUTDOWN_SEND, virtio_vsock_type_VIRTIO_VSOCK_TYPE_STREAM,
    VIRTIO_F_VERSION_1,
};

use super::{
    ActivateError, ActivateResult, DescriptorChain, EpollHandlerPayload, Queue, VirtioDevice,
    TYPE_VSOCK, VIRTIO_MMIO_INT_VRING,
};
use super::super::Error as DeviceError;
use super::super::{DeviceEventT, EpollHandler};
use self::epoll_handler::VsockEpollHandler;
use self::muxer::VsockMuxer;
use self::connection::VsockConnection;
use std::collections::VecDeque;


// The guest has made a buffer available to receive a frame into.
const RX_QUEUE_EVENT: DeviceEventT = 0;
// The transmit queue has a frame that is ready to send from the guest.
const TX_QUEUE_EVENT: DeviceEventT = 1;
// rx rate limiter budget is now available.
const EVENT_QUEUE_EVENT: DeviceEventT = 2;
const MUXER_EVENT: DeviceEventT = 3;

// Number of DeviceEventT events supported by this implementation.
pub const VSOCK_EVENTS_COUNT: usize = 4;

const QUEUE_SIZE: u16 = 256;
const NUM_QUEUES: usize = 3;
const QUEUE_SIZES: &'static [u16] = &[QUEUE_SIZE; NUM_QUEUES];

//const MAX_PKT_BUF_SIZE: usize = 65536;
const VSOCK_TX_BUF_SIZE: usize = 256*1024;

const TEMP_VSOCK_PATH: &str = "./vsock";

// TODO: clean this up. Perhaps discard bindgen for vsock altogether
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

const VIRTIO_F_IN_ORDER: usize = 35;

#[derive(Debug)]
pub enum VsockError {
    PacketAssemblyError,
    GeneralError,
}
type Result<T> = std::result::Result<T, VsockError>;


pub struct EpollConfig {
    rx_queue_token: u64,
    tx_queue_token: u64,
    ev_queue_token: u64,
    muxer_token: u64,
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
            muxer_token: first_token + MUXER_EVENT as u64,
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
            avail_features: 1u64 << VIRTIO_F_VERSION_1 | 1u64 << VIRTIO_F_IN_ORDER,
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
        let muxer_epoll_rawfd = epoll::create(true).map_err(ActivateError::EpollCtl)?;

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
            muxer: VsockMuxer::new(self.cid, muxer_epoll_rawfd),
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

        epoll::ctl(
            self.epoll_config.epoll_raw_fd,
            epoll::ControlOptions::EPOLL_CTL_ADD,
            muxer_epoll_rawfd,
            epoll::Event::new(epoll::Events::EPOLLIN, self.epoll_config.muxer_token),
        ).map_err(ActivateError::EpollCtl)?;



        Ok(())
    }
}

