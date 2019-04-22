// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//

use std::cmp::min;
use std::mem;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use memory_model::GuestMemory;
use sys_util::EventFd;

use super::super::queue::Queue as VirtQueue;
use super::muxer::VsockMuxer;
use super::packet::{VsockPacket, VsockPacketHdr};
use super::*;


pub struct VsockEpollHandler {
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
    pub muxer: VsockMuxer,
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
                match pkt.write_to_virtq_head(&head, &self.mem) {
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
            let pkt = match VsockPacket::from_virtq_head(&head, &self.mem) {
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
            self.muxer.send(pkt);
            self.txvq.add_used(&self.mem, head.index, 0);
        }

        self.process_rx();
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
                    return Err(DeviceError::FailedReadingQueue {
                        event_type: "rx queue event",
                        underlying: e,
                    });
                } else {
                    self.process_rx();
                }
            }
            TX_QUEUE_EVENT => {
                if let Err(e) = self.txvq_evt.read() {
                    error!("Failed to get tx queue event: {:?}", e);
                    return Err(DeviceError::FailedReadingQueue {
                        event_type: "tx queue event",
                        underlying: e,
                    });
                } else {
                    self.process_tx();
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
            }
            MUXER_EVENT => {
                debug!("vsock: received muxer event");
                self.muxer.kick();
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

