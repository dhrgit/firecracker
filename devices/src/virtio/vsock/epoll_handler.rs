// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//

use std::result;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use memory_model::GuestMemory;
use sys_util::EventFd;

use super::super::super::{DeviceEventT, Error as DeviceError};
use super::super::queue::Queue as VirtQueue;
use super::super::{EpollHandlerPayload, VIRTIO_MMIO_INT_VRING};
use super::{EpollHandler, VsockBackend};
use super::packet::VsockPacket;
use super::defs;


pub struct VsockEpollHandler<B: VsockBackend> {
    pub rxvq: VirtQueue,
    pub rxvq_evt: EventFd,
    pub txvq: VirtQueue,
    pub txvq_evt: EventFd,
    pub evq: VirtQueue,
    pub evq_evt: EventFd,
    pub cid: u64,
    pub mem: GuestMemory,
    pub interrupt_status: Arc<AtomicUsize>,
    pub interrupt_evt: EventFd,
    pub backend: B,
}

impl<B> VsockEpollHandler<B> where B: VsockBackend {
    fn signal_used_queue(&self) -> result::Result<(), DeviceError> {
        debug!("vsock: raising IRQ");
        self.interrupt_status
            .fetch_or(VIRTIO_MMIO_INT_VRING as usize, Ordering::SeqCst);
        self.interrupt_evt.write(1).map_err(|e| {
            error!("Failed to signal used queue: {:?}", e);
            DeviceError::FailedSignalingUsedQueue(e)
        })
    }

    fn process_rx(&mut self) {

        debug!("vsock: epoll_handler::process_rx()");

        let mut raise_irq = false;

        while let Some(head) = self.rxvq.iter(&self.mem).next() {
            let mut max_len = 0usize;

            let mut maybe_desc = head.next_descriptor();
            while let Some(desc) = maybe_desc {
                max_len += desc.len as usize;
                maybe_desc = desc.next_descriptor();
            }

            if let Some(pkt) = self.backend.recv_pkt(max_len) {
                let len = match pkt.write_to_virtq_head(&head, &self.mem) {
                    Err(e) => {
                        warn!("vsock: error writing pkt to guest mem: {:?}", e);
                        self.rxvq.go_to_previous_position();
                        break;
                    }
                    Ok(len) => len,
                };
                raise_irq = true;
                self.rxvq.add_used(&self.mem, head.index, len as u32);
            } else {
                self.rxvq.go_to_previous_position();
                break;
            }
        }

        if raise_irq {
            self.signal_used_queue().unwrap_or_default();
        }
    }

    fn process_tx(&mut self) {

        debug!("vsock: epoll_handler::process_tx()");

        let mut raise_irq = false;

        while let Some(head) = self.txvq.iter(&self.mem).next() {
            let pkt = match VsockPacket::from_virtq_head(&head, &self.mem) {
                Ok(pkt) => pkt,
                Err(e) => {
                    error!("vsock: error reading TX packet: {:?}", e);
                    raise_irq = true;
                    self.txvq.add_used(&self.mem, head.index, 0);
                    continue;
                }
            };

            if self.backend.send_pkt(pkt) != true {
                self.txvq.go_to_previous_position();
                break;
            }

            raise_irq = true;
            self.txvq.add_used(&self.mem, head.index, 0);
        }

        if raise_irq {
            self.signal_used_queue().unwrap_or_default();
        }
    }

}

impl<B> EpollHandler for VsockEpollHandler<B> where B: VsockBackend {
    fn handle_event(
        &mut self,
        device_event: DeviceEventT,
        evmask: u32,
        _payload: EpollHandlerPayload,
    ) -> result::Result<(), DeviceError> {
        match device_event {
            defs::RXQ_EVENT => {
                info!("vsock RX q event");
                if let Err(e) = self.rxvq_evt.read() {
                    error!("Failed to get rx queue event: {:?}", e);
                    return Err(DeviceError::FailedReadingQueue {
                        event_type: "rx queue event",
                        underlying: e,
                    });
                } else {
                    self.process_rx();
                }
            }
            defs::TXQ_EVENT => {
                info!("vsock TX q event");
                if let Err(e) = self.txvq_evt.read() {
                    error!("Failed to get tx queue event: {:?}", e);
                    return Err(DeviceError::FailedReadingQueue {
                        event_type: "tx queue event",
                        underlying: e,
                    });
                } else {
                    self.process_tx();
                    self.process_rx();
                }
            }
            defs::EVQ_EVENT => {
                warn!("Event queue unimplemented");
                if let Err(e) = self.evq_evt.read() {
                    error!("Failed to consume evq event: {:?}", e);
                    return Err(DeviceError::FailedReadingQueue {
                        event_type: "ev queue event",
                        underlying: e,
                    });
                }
            }
            defs::BACKEND_EVENT => {
                debug!("vsock: received backend event");
                if let Some(evset) = epoll::Events::from_bits(evmask) {
                    self.backend.notify(evset);
                }
                else {
                    error!("vsock: invalid backend event, evmask={}", evmask);
                }

                // This event may have caused some packets to be queued up by the backend.
                // Make sure they are processed.
                self.process_rx();
            }
            other => {
                return Err(DeviceError::UnknownEvent {
                    device: "vsock",
                    event: other,
                });
            },
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {

    use std::collections::VecDeque;
    use super::super::Result;
    use super::super::defs::uapi;
    use super::super::packet::VsockPacket;


    pub struct DummyMuxer {
        pub rxq: VecDeque<VsockPacket>,
        fwd_cnt: usize,
        credit_update_counter: usize,
    }

    impl DummyMuxer {
        pub fn new() -> Self {
            Self {
                rxq: VecDeque::new(),
                fwd_cnt: 0,
                credit_update_counter: 0,
            }
        }

        fn send(&mut self, pkt: VsockPacket) -> Result<()> {

            self.fwd_cnt += pkt.hdr.len as usize;

            debug!("mock TX (rxq={}): op={}, len={}, ba={}, fc={}",
                   self.rxq.len(),
                   pkt.hdr.op,
                   pkt.hdr.len,
                   pkt.hdr.buf_alloc,
                   pkt.hdr.fwd_cnt,
            );

            let mut re_pkt = VsockPacket::new_response(
                pkt.hdr.dst_cid,
                pkt.hdr.src_cid,
                pkt.hdr.dst_port,
                pkt.hdr.src_port
            );

            re_pkt.hdr.buf_alloc = 256*1024;
            re_pkt.hdr.fwd_cnt = self.fwd_cnt as u32;

            match pkt.hdr.op {
                uapi::VSOCK_OP_REQUEST => {
                    self.rxq.push_back(re_pkt);
                },
                uapi::VSOCK_OP_RW => {
                    self.credit_update_counter += 1;
                    if self.credit_update_counter > 15 {
                        self.credit_update_counter = 0;
                        re_pkt.hdr.op = uapi::VSOCK_OP_CREDIT_UPDATE;
                        self.rxq.push_back(re_pkt);
                    }
                },
                uapi::VSOCK_OP_CREDIT_REQUEST => {
                    re_pkt.hdr.op = uapi::VSOCK_OP_CREDIT_UPDATE;
                    self.rxq.push_back(re_pkt);
                },
                uapi::VSOCK_OP_SHUTDOWN => {
                    re_pkt.hdr.op = uapi::VSOCK_OP_RST;
                    self.rxq.push_back(re_pkt);
                },
                _ => {
                    debug!("mock: unexpected TX pkt: op={} len={}", pkt.hdr.op, pkt.hdr.len);
                }
            }
            Ok(())
        }

        fn recv(&mut self, max_len: usize) -> Option<VsockPacket> {
            let mp = self.rxq.pop_front();
            match mp {
                Some(ref p) => debug!(
                    "mock RX (rxq={}): op={} len={} ba={} fc={}",
                    self.rxq.len(), p.hdr.op, p.hdr.len, p.hdr.buf_alloc, p.hdr.fwd_cnt
                ),
                None => debug!("mock RX (empty)"),
            }
            mp
        }

        fn kick(&self) {
            debug!("mock kick");
        }
    }
}
