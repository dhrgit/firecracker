// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

mod device;
mod epoll_handler;
mod packet;
mod unix;

pub use self::device::Vsock;
pub use self::defs::EVENT_COUNT as VSOCK_EVENTS_COUNT;

use std::sync::mpsc;
use std::os::unix::io::{RawFd, AsRawFd};

use super::super::EpollHandler;
use self::packet::VsockPacket;


mod defs {
    use super::super::super::DeviceEventT;

    /// RX queue event: the driver added available buffers to the RX queue.
    pub const RXQ_EVENT: DeviceEventT = 0;
    /// TX queue event: the driver added available buffers to the RX queue.
    pub const TXQ_EVENT: DeviceEventT = 1;
    /// Event queue event: the driver added available buffers to the event queue.
    pub const EVQ_EVENT: DeviceEventT = 2;
    /// Backend event: the backend needs a kick.
    pub const BACKEND_EVENT: DeviceEventT = 3;
    /// Total number of events known to the vsock epoll handler.
    pub const EVENT_COUNT: usize = 4;

    pub const QUEUE_SIZE: u16 = 256;
    pub const NUM_QUEUES: usize = 3;
    pub const QUEUE_SIZES: &'static [u16] = &[QUEUE_SIZE; NUM_QUEUES];

    /// Max vsock packet data/buffer size.
    pub const MAX_PKT_BUF_SIZE: usize = 512 * 1024;

    pub mod uapi {
        use virtio_gen::virtio_vsock as gen;

        pub const VIRTIO_F_IN_ORDER: usize = 35;
        pub const VIRTIO_F_VERSION_1: u32 = gen::VIRTIO_F_VERSION_1;

        pub const VIRTIO_ID_VSOCK: u32 = gen::VIRTIO_ID_VSOCK;

        pub const VSOCK_OP_CREDIT_REQUEST: u16 = gen::virtio_vsock_op_VIRTIO_VSOCK_OP_CREDIT_REQUEST as u16;
        pub const VSOCK_OP_CREDIT_UPDATE: u16 = gen::virtio_vsock_op_VIRTIO_VSOCK_OP_CREDIT_UPDATE as u16;
        pub const VSOCK_OP_REQUEST: u16 = gen::virtio_vsock_op_VIRTIO_VSOCK_OP_REQUEST as u16;
        pub const VSOCK_OP_RESPONSE: u16 = gen::virtio_vsock_op_VIRTIO_VSOCK_OP_RESPONSE as u16;
        pub const VSOCK_OP_RST: u16 = gen::virtio_vsock_op_VIRTIO_VSOCK_OP_RST as u16;
        pub const VSOCK_OP_RW: u16 = gen::virtio_vsock_op_VIRTIO_VSOCK_OP_RW as u16;
        pub const VSOCK_OP_SHUTDOWN: u16 = gen::virtio_vsock_op_VIRTIO_VSOCK_OP_SHUTDOWN as u16;

        pub const VSOCK_FLAGS_SHUTDOWN_RCV: u32 = gen::virtio_vsock_shutdown_VIRTIO_VSOCK_SHUTDOWN_RCV as u32;
        pub const VSOCK_FLAGS_SHUTDOWN_SEND: u32 = gen::virtio_vsock_shutdown_VIRTIO_VSOCK_SHUTDOWN_SEND as u32;

        pub const VSOCK_TYPE_STREAM: u16 = gen::virtio_vsock_type_VIRTIO_VSOCK_TYPE_STREAM as u16;

        pub const VSOCK_HOST_CID: u64 = 2;
    }
}


#[derive(Debug)]
pub enum VsockError {
    PacketAssemblyError,
    GeneralError,
}
type Result<T> = std::result::Result<T, VsockError>;


pub struct EpollConfig {
    rxq_token: u64,
    txq_token: u64,
    evq_token: u64,
    backend_token: u64,
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
            rxq_token: first_token + defs::RXQ_EVENT as u64,
            txq_token: first_token + defs::TXQ_EVENT as u64,
            evq_token: first_token + defs::EVQ_EVENT as u64,
            backend_token: first_token + defs::BACKEND_EVENT as u64,
            epoll_raw_fd,
            sender,
        }
    }
}

pub trait VsockBackend : Send {
    fn get_epoll_listener(&self) -> (RawFd, epoll::Events);
    fn notify(&mut self, evset: epoll::Events);
    fn send_pkt(&mut self, pkt: VsockPacket) -> bool;
    fn recv_pkt(&mut self, max_len: usize) -> Option<VsockPacket>;
}

