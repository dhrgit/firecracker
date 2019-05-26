// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//

/// `MuxerRxQ` implements a helper object that `VsockMuxer` can use for queuing RX (host -> guest)
/// packets (or rather instructions on how to build said packets).
///
/// Under ideal operation, every connection that has pending RX data will be present in the muxer RX
/// queue. However, since the RX queue is smaller than the connection pool, it may, under some conditions,
/// become full, meaning that it can no longer account for all the connections that can yield RX data.
/// When that happens, we say that it is no longer "synchronized" (i.e. with the connection pool).
/// A desynchronized RX queue still holds valid data, and the muxer will continue to pop packets from
/// it. However, when a desynchronized queue is drained, additional data may still be available, so the
/// muxer will have to perform a more costly walk of the entire connection pool to find it.
/// This walk is also implemented here, and it is part of the resynchronization procedure: inspect all
/// connections, and add every connection that has pending RX data to the RX queue.
///
use std::collections::{HashMap, VecDeque};

use super::super::VsockChannel;
use super::connection::VsockConnection;
use super::defs as unix_defs;
use super::muxer::{ConnMapKey, MuxerRx};
use super::{Error, Result};

/// The muxer RX queue.
///
pub struct MuxerRxQ {
    /// The RX queue data.
    q: VecDeque<MuxerRx>,
    /// The RX queue sync status.
    synced: bool,
}

impl MuxerRxQ {
    const SIZE: usize = unix_defs::MUXER_RXQ_SIZE;

    /// Trivial RX queue constructor.
    ///
    pub fn new() -> Self {
        Self {
            q: VecDeque::with_capacity(Self::SIZE),
            synced: true,
        }
    }

    /// Push a new RX item to the queue.
    ///
    /// A push will fail when:
    /// - trying to push a connection key onto an out-of-sync, or full queue; or
    /// - trying to push an RST onto a queue already full of RSTs.
    /// RSTs take precedence over connections, because connections can always be queried for pending
    /// RX data later. Aside from this queue, there is no other storage for RSTs, so, failing to
    /// push one means that we have to drop the packet.
    ///
    /// Returns:
    /// - `Ok(())` if the new item has been successfully queued; or
    /// - `Err(Error::QueueFull)` if there was no room left in the queue.
    ///
    pub fn push(&mut self, rx: MuxerRx) -> Result<()> {
        // Pushing to a non-full, synchronized queue will always succeed.
        if self.is_synced() && !self.is_full() {
            self.q.push_back(rx);
            return Ok(());
        }

        match rx {
            MuxerRx::RstPkt { .. } => {
                // If we just failed to push an RST packet, we'll look through the queue, trying to
                // find a connection key that we could evict. This way, the queue does lose sync, but
                // we don't drop any packets.
                for qi in self.q.iter_mut().rev() {
                    if let MuxerRx::ConnRx(_) = qi {
                        *qi = rx;
                        self.synced = false;
                        return Ok(());
                    }
                }
            }
            MuxerRx::ConnRx(_) => {
                self.synced = false;
            }
        };

        Err(Error::QueueFull)
    }

    /// Pop an RX item from the front of the queue.
    ///
    pub fn pop(&mut self) -> Option<MuxerRx> {
        self.q.pop_front()
    }

    /// Attempt to synchronize the RX queue with the connection pool.
    ///
    /// Returns:
    /// - true, if the queue has been synced (i.e. all connections, that have some RX pending, have
    ///   been added to the queue); or
    /// - false, it the queue is still out-of-sync.
    ///
    pub fn try_sync(&mut self, conn_map: &HashMap<ConnMapKey, VsockConnection>) -> bool {
        for (key, conn) in conn_map.iter() {
            if conn.has_pending_rx() && self.push(MuxerRx::ConnRx(*key)).is_err() {
                return false;
            }
        }
        self.synced = true;
        true
    }

    /// Check if the RX queue is synchronized with the connection pool.
    ///
    pub fn is_synced(&self) -> bool {
        self.synced
    }

    /// Get the total number of items in the queue.
    ///
    pub fn len(&self) -> usize {
        self.q.len()
    }

    /// Check if the queue is empty.
    ///
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Check if the queue is full.
    ///
    pub fn is_full(&self) -> bool {
        self.len() == Self::SIZE
    }
}
