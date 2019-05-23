// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

/// The vsock `EpollHandler` implements the runtime logic of our vsock device:
/// 1. Respond to TX queue events by wrapping virtio buffers into `VsockPacket`s, then sending those
///    packets to the `VsockBackend`;
/// 2. Forward backend FD event notifications to the `VsockBackend`;
/// 3. Fetch incoming packets from the `VsockBackend` and place them into the virtio RX queue;
/// 4. Whenever we have processed some virtio buffers (either TX or RX), let the driver know by
///    raising our assigned IRQ.
///
/// In a nutshell, the `EpollHandler` logic looks like this:
/// - on TX queue event:
///   - fetch all packets from the TX queue and send them to the backend; then
///   - if the backend has queued up any incoming packets, fetch them into any available RX buffers.
/// - on RX queue event:
///   - fetch any incoming packets, queued up by the backend, into newly available RX buffers.
/// - on backend event:
///   - forward the event to the backend; then
///   - again, attempt to fetch any incoming packets queued by the backend into virtio RX buffers.
///
use std::result;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use memory_model::GuestMemory;
use sys_util::EventFd;

use super::super::super::{DeviceEventT, Error as DeviceError};
use super::super::queue::Queue as VirtQueue;
use super::super::{EpollHandlerPayload, VIRTIO_MMIO_INT_VRING};
use super::defs;
use super::packet::VsockPacket;
use super::{EpollHandler, VsockBackend};

// TODO: Detect / handle queue deadlock:
// 1. If `self.backend.send_pkt()` errors out, TX queue processing will halt. Try to process any
//    pending backend RX, then try TX again. If it fails again, we have a deadlock.
// 2. If the driver halts RX queue processing, we'll need to notify `self.backend`, so that it
//    can unregister any EPOLLIN listeners, since otherwise it will keep spinning, unable to consume
//    its EPOLLIN events.

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

impl<B> VsockEpollHandler<B>
where
    B: VsockBackend,
{
    /// Signal the guest driver that we've used some virtio buffers that it had previously made
    /// available.
    ///
    fn signal_used_queue(&self) -> result::Result<(), DeviceError> {
        debug!("vsock: raising IRQ");
        self.interrupt_status
            .fetch_or(VIRTIO_MMIO_INT_VRING as usize, Ordering::SeqCst);
        self.interrupt_evt.write(1).map_err(|e| {
            error!("Failed to signal used queue: {:?}", e);
            DeviceError::FailedSignalingUsedQueue(e)
        })
    }

    /// Walk the driver-provided RX queue buffers and attempt to fill them up with any data that we
    /// have pending.
    ///
    fn process_rx(&mut self) {
        debug!("vsock: epoll_handler::process_rx()");

        let mut raise_irq = false;

        while let Some(head) = self.rxvq.iter(&self.mem).next() {
            let used_len = match VsockPacket::from_rx_virtq_head(&head) {
                Ok(mut pkt) => {
                    if self.backend.recv_pkt(&mut pkt).is_ok() {
                        pkt.hdr.as_slice().len() as u32 + pkt.hdr.len
                    } else {
                        // We are using a consuming iterator over the virtio buffers, so, if we can't
                        // fill in this buffer, we'll need to undo the last iterator step.
                        self.rxvq.go_to_previous_position();
                        break;
                    }
                }
                Err(e) => {
                    warn!("vsock: RX queue error: {:?}", e);
                    0
                }
            };

            raise_irq = true;
            self.rxvq.add_used(&self.mem, head.index, used_len);
        }

        if raise_irq {
            self.signal_used_queue().unwrap_or_default();
        }
    }

    /// Walk the dirver-provided TX queue buffers, package them up as vsock packets, and send them to
    /// the backend for processing.
    ///
    fn process_tx(&mut self) {
        debug!("vsock: epoll_handler::process_tx()");

        let mut have_used = false;

        while let Some(head) = self.txvq.iter(&self.mem).next() {
            let pkt = match VsockPacket::from_tx_virtq_head(&head) {
                Ok(pkt) => pkt,
                Err(e) => {
                    error!("vsock: error reading TX packet: {:?}", e);
                    have_used = true;
                    self.txvq.add_used(&self.mem, head.index, 0);
                    continue;
                }
            };

            if self.backend.send_pkt(&pkt).is_err() {
                self.txvq.go_to_previous_position();
                break;
            }

            have_used = true;
            self.txvq.add_used(&self.mem, head.index, 0);
        }

        if have_used {
            self.signal_used_queue().unwrap_or_default();
        }
    }
}

impl<B> EpollHandler for VsockEpollHandler<B>
where
    B: VsockBackend,
{
    /// Respond to a new event, coming from the main epoll loop (implemented by the VMM).
    ///
    fn handle_event(
        &mut self,
        device_event: DeviceEventT,
        evset_bits: u32,
        _payload: EpollHandlerPayload,
    ) -> result::Result<(), DeviceError> {
        match device_event {
            defs::RXQ_EVENT => {
                debug!("vsock: RX queue event");
                if let Err(e) = self.rxvq_evt.read() {
                    error!("Failed to get rx queue event: {:?}", e);
                    return Err(DeviceError::FailedReadingQueue {
                        event_type: "rx queue event",
                        underlying: e,
                    });
                } else {
                    if self.backend.has_pending_rx() {
                        self.process_rx();
                    }
                }
            }
            defs::TXQ_EVENT => {
                debug!("vsock: TX queue event");
                if let Err(e) = self.txvq_evt.read() {
                    error!("Failed to get tx queue event: {:?}", e);
                    return Err(DeviceError::FailedReadingQueue {
                        event_type: "tx queue event",
                        underlying: e,
                    });
                } else {
                    self.process_tx();
                    // The backend may have queued up responses to the packets we sent during TX queue
                    // processing. If that happened, we need to fetch those responses and place them
                    // into RX buffers.
                    if self.backend.has_pending_rx() {
                        self.process_rx();
                    }
                }
            }
            defs::EVQ_EVENT => {
                debug!("vsock: event queue event");
                if let Err(e) = self.evq_evt.read() {
                    error!("Failed to consume evq event: {:?}", e);
                    return Err(DeviceError::FailedReadingQueue {
                        event_type: "ev queue event",
                        underlying: e,
                    });
                }
            }
            defs::BACKEND_EVENT => {
                debug!("vsock: backend event");
                if let Some(evset) = epoll::Events::from_bits(evset_bits) {
                    self.backend.notify(evset);
                    // This event may have caused some packets to be queued up by the backend.
                    // Make sure they are processed.
                    if self.backend.has_pending_rx() {
                        self.process_rx();
                    }
                } else {
                    warn!("vsock: unexpected backend event flags={:08x}", evset_bits);
                }
            }
            other => {
                return Err(DeviceError::UnknownEvent {
                    device: "vsock",
                    event: other,
                });
            }
        }

        Ok(())
    }
}
