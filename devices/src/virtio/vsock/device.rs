// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::os::unix::io::{AsRawFd};
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use byteorder::{ByteOrder, LittleEndian};

use memory_model::GuestMemory;
use sys_util::EventFd;

use super::super::{ActivateError, ActivateResult, Queue as VirtQueue, VirtioDevice};
use super::{VsockBackend};
use super::epoll_handler::VsockEpollHandler;
use super::{EpollConfig, defs, defs::uapi};


pub struct Vsock<B: VsockBackend> {
    cid: u64,
    backend: Option<B>,
    avail_features: u64,
    acked_features: u64,
    config_space: Vec<u8>,
    epoll_config: EpollConfig,
}

impl<B> Vsock<B>
    where B: VsockBackend
{
    /// Create a new virtio-vsock device with the given VM cid.
    pub fn new(
        cid: u64,
        epoll_config: EpollConfig,
        backend: B,
    ) -> super::Result<Vsock<B>> {

        Ok(Vsock {
            cid,
            avail_features: 1u64 << uapi::VIRTIO_F_VERSION_1 | 1u64 << uapi::VIRTIO_F_IN_ORDER,
            acked_features: 0,
            config_space: Vec::new(),
            epoll_config,
            backend: Some(backend),
        })
    }
}

impl<B> VirtioDevice for Vsock<B>
    where B: VsockBackend + 'static
{
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

        let backend = self.backend.take().unwrap();
        let backend_fd = backend.get_polled_fd();
        let backend_evset = backend.get_polled_evset();

        let handler: VsockEpollHandler<B> = VsockEpollHandler {
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
            backend,
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
            backend_fd,
            epoll::Event::new(backend_evset, self.epoll_config.backend_token),
        ).map_err(ActivateError::EpollCtl)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {

    use std::collections::VecDeque;
    use std::fs::File;
    use std::num::Wrapping;
    use std::io::Write;

    use super::*;
    use crate::virtio::vsock::{VsockChannel, VsockEpollListener, VsockBackend};
    use crate::virtio::vsock::packet::{VsockPacket, VsockPacketBuf, VsockPacketHdr};

    pub struct DummyBackend {
        sink: File,
        rxq: VecDeque<VsockPacketHdr>,
        fwd_cnt: Wrapping<u32>,
    }

    impl DummyBackend {
        pub fn new() -> Self {
            Self {
                sink: File::create("/dev/null").unwrap(),
                rxq: VecDeque::new(),
                fwd_cnt: Wrapping(0),
            }
        }
    }

    impl VsockEpollListener for DummyBackend {
        fn get_polled_fd(&self) -> RawFd {
            -1 as RawFd
        }
        fn get_polled_evset(&self) -> epoll::Events {
            epoll::Events::empty()
        }
        fn notify(&mut self, evset: epoll::Events) {}
    }

    impl VsockChannel for DummyBackend {
        fn recv_pkt(&mut self, buf: VsockPacketBuf) -> Option<VsockPacket> {
            self.rxq.pop_front()
                .map(|hdr| {
                    VsockPacket { hdr, buf: None }
                })
        }
        fn send_pkt(&mut self, pkt: &VsockPacket) -> bool {
            debug!("dummy muxer send pkt: {:?}", pkt.hdr);
            match pkt.hdr.op {
                uapi::VSOCK_OP_REQUEST => {
                    self.rxq.push_back(
                        VsockPacketHdr {
                            src_cid: pkt.hdr.dst_cid,
                            dst_cid: pkt.hdr.src_cid,
                            src_port: pkt.hdr.dst_port,
                            dst_port: pkt.hdr.src_port,
                            op: uapi::VSOCK_OP_RESPONSE,
                            type_: uapi::VSOCK_TYPE_STREAM,
                            buf_alloc: 256 * 1024,
                            fwd_cnt: self.fwd_cnt.0,
                            .. Default::default()
                        }
                    );
                },
                uapi::VSOCK_OP_RW => {
                    if let Some(buf) = pkt.buf.as_ref() {
                        self.sink.write(&buf.as_slice()[..pkt.hdr.len as usize]).unwrap();
                        self.fwd_cnt += Wrapping(pkt.hdr.len);
                        self.rxq.push_back(VsockPacketHdr {
                                src_cid: pkt.hdr.dst_cid,
                                dst_cid: pkt.hdr.src_cid,
                                src_port: pkt.hdr.dst_port,
                                dst_port: pkt.hdr.src_port,
                                op: uapi::VSOCK_OP_CREDIT_UPDATE,
                                type_: uapi::VSOCK_TYPE_STREAM,
                                buf_alloc: 256 * 1024,
                                fwd_cnt: self.fwd_cnt.0,
                                .. Default::default()
                        });
                    }
                },
                uapi::VSOCK_OP_SHUTDOWN => {
                    self.rxq.push_back(VsockPacketHdr {
                        src_cid: pkt.hdr.dst_cid,
                        dst_cid: pkt.hdr.src_cid,
                        src_port: pkt.hdr.dst_port,
                        dst_port: pkt.hdr.src_port,
                        op: uapi::VSOCK_OP_RST,
                        type_: uapi::VSOCK_TYPE_STREAM,
                        buf_alloc: 256 * 1024,
                        fwd_cnt: self.fwd_cnt.0,
                        .. Default::default()
                    });
                },
                _ => ()
            }
            true
        }
    }

    impl VsockBackend for DummyBackend {}

}

