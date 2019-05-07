// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
use std::os::unix::net::UnixListener;
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use byteorder::{ByteOrder, LittleEndian};

use memory_model::GuestMemory;
use sys_util::EventFd;

use super::super::{ActivateError, ActivateResult, Queue as VirtQueue, VirtioDevice};
use super::VsockError;
use super::epoll_handler::VsockEpollHandler;
use super::unix::muxer::VsockMuxer;
use super::{EpollConfig, defs, defs::uapi};


pub struct Vsock {
    cid: u64,
    host_sock: Option<UnixListener>,
    host_sock_path: Option<String>,
    backend_epoll_fd: RawFd,
    avail_features: u64,
    acked_features: u64,
    config_space: Vec<u8>,
    epoll_config: EpollConfig,
}

impl Vsock {
    /// Create a new virtio-vsock device with the given VM cid.
    pub fn new(
        cid: u64,
        host_sock_path: String,
        epoll_config: EpollConfig
    ) -> super::Result<Vsock> {

        // TODO: report these possible errors more accurately
        let host_sock = UnixListener::bind(&host_sock_path)
            .and_then(|sock| {
                sock.set_nonblocking(true).map(|_| sock)
            })
            .map_err(VsockError::IoError)?;
        let backend_epoll_fd = epoll::create(true)
            .map_err(VsockError::IoError)?;

        Ok(Vsock {
            cid,
            host_sock: Some(host_sock),
            host_sock_path: Some(host_sock_path),
            backend_epoll_fd,
            avail_features: 1u64 << uapi::VIRTIO_F_VERSION_1 | 1u64 << uapi::VIRTIO_F_IN_ORDER,
            acked_features: 0,
            config_space: Vec::new(),
            epoll_config,
        })
    }
}

impl VirtioDevice for Vsock {
    fn device_type(&self) -> u32 {
        uapi::VIRTIO_ID_VSOCK
    }

    fn queue_max_sizes(&self) -> &[u16] {
        defs::QUEUE_SIZES
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
        mut queues: Vec<VirtQueue>,
        mut queue_evts: Vec<EventFd>,
    ) -> ActivateResult {
        if queues.len() != defs::NUM_QUEUES || queue_evts.len() != defs::NUM_QUEUES {
            error!(
                "Cannot perform activate. Expected {} queue(s), got {}",
                defs::NUM_QUEUES,
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
        let muxer = VsockMuxer::new(
            self.cid,
            self.host_sock.take().unwrap(),
            self.host_sock_path.take().unwrap(),
            self.backend_epoll_fd
        ).map_err(|err| {
                ActivateError::VsockError(VsockError::BackendError(
                    format!("{:?}", err)
                ))
            })?;

        let handler: VsockEpollHandler<VsockMuxer> = VsockEpollHandler {
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
            backend: muxer,
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
            epoll::Event::new(epoll::Events::EPOLLIN, self.epoll_config.rxq_token),
        )
            .map_err(ActivateError::EpollCtl)?;

        epoll::ctl(
            self.epoll_config.epoll_raw_fd,
            epoll::ControlOptions::EPOLL_CTL_ADD,
            tx_queue_rawfd,
            epoll::Event::new(epoll::Events::EPOLLIN, self.epoll_config.txq_token),
        )
            .map_err(ActivateError::EpollCtl)?;

        epoll::ctl(
            self.epoll_config.epoll_raw_fd,
            epoll::ControlOptions::EPOLL_CTL_ADD,
            ev_queue_rawfd,
            epoll::Event::new(epoll::Events::EPOLLIN, self.epoll_config.evq_token),
        )
            .map_err(ActivateError::EpollCtl)?;

        epoll::ctl(
            self.epoll_config.epoll_raw_fd,
            epoll::ControlOptions::EPOLL_CTL_ADD,
            self.backend_epoll_fd,
            epoll::Event::new(epoll::Events::EPOLLIN, self.epoll_config.backend_token),
        ).map_err(ActivateError::EpollCtl)?;



        Ok(())
    }
}

