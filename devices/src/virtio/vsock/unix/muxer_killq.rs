// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//

/// `MuxerKillQ` implements a helper object that `VsockMuxer` can use for scheduling forced connection
/// termination. I.e. after one peer issues a clean shutdown request (VSOCK_OP_SHUTDOWN), the concerned
/// connection is queued for termination (VSOCK_OP_RST) in the near future (herein implemented via
/// an expiring timer).
///
/// Whenever the muxer needs to schedule a connection for termination, it pushes it (or rather an
/// identifier - the connection key) to this queue. A subsequent pop() operation will succeed if and
/// only if the first connection in the queue is ready to be terminated (i.e. its kill timer expired).
///
/// Without using this queue, the muxer would have to walk its entire connection pool (hashmap), whenever
/// it needs to check for expired kill timers. With this queue, both scheduling and termination are
/// performed in constant time. However, since we don't want to waste space on a kill queue that's
/// as big as the connection hashmap itself, it is possible that this queue may become full at times.
/// We call this kill queue "synchronized" if we are certain that all connections that are awaiting
/// termination are present in the queue. This means a simple constant-time pop() operation is enough
/// to check wether any connections need to be terminated.
/// When the kill queue becomes full, though, pushing fails, so connections that should be terminated
/// are left out. The queue is not synchronized anymore. When that happens, the muxer will first drain
/// the queue, and then attempt to synchronize it, by walking the connection pool and pushing any
/// connection that is scheduled for termination to this queue.
///
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use super::connection::VsockConnection;
use super::defs as unix_defs;
use super::muxer::ConnMapKey;

/// A kill queue item, holding the connection key and the scheduled time for termination.
///
#[derive(Clone, Copy, Debug)]
struct MuxerKillQItem {
    key: ConnMapKey,
    kill_time: Instant,
}

/// The connection kill queue: a FIFO structure, storing the connections that are scheduled for
/// termination.
///
pub struct MuxerKillQ {
    /// The kill queue contents.
    q: VecDeque<MuxerKillQItem>,

    /// The kill queue sync status:
    /// - when true, all connections that are awaiting termination are guaranteed to be in this queue;
    /// - when false, some connections may have been left out.
    ///
    synced: bool,
}

impl MuxerKillQ {
    const SIZE: usize = unix_defs::MUXER_KILLQ_SIZE;

    /// Trivial kill queue constructor.
    ///
    pub fn new() -> Self {
        Self {
            q: VecDeque::with_capacity(Self::SIZE),
            synced: true,
        }
    }

    /// Push a connection key to the queue, scheduling it for termination at `CONN_SHUTDOWN_TIMEOUT_MS`
    /// from now (the push time).
    ///
    pub fn push(&mut self, key: ConnMapKey) {
        if !self.is_synced() || self.is_full() {
            return;
        }
        self.q.push_back(MuxerKillQItem {
            key,
            kill_time: Instant::now() + Duration::from_millis(unix_defs::CONN_SHUTDOWN_TIMEOUT_MS),
        });
    }

    /// Attempt to pop an expired connection from the kill queue.
    ///
    /// This will succeed and return a connection key, only if the connection at the front of the queue
    /// has expired. Otherwise, `None` is returned.
    ///
    pub fn pop(&mut self) -> Option<ConnMapKey> {
        if let Some(item) = self.q.front() {
            if Instant::now() > item.kill_time {
                return Some(self.q.pop_front().unwrap().key);
            }
        }
        None
    }

    /// Attempt to synchronize the kill queue.
    ///
    /// This will walk the connection pool, looking for connections that should be scheduled for
    /// termination, but are not present in the queue.
    /// Returns:
    /// - true, if the queue was successfuly synchornized; or
    /// - false, if the queue is still out-of-sync.
    ///
    pub fn try_sync(&mut self, conn_map: &HashMap<ConnMapKey, VsockConnection>) -> bool {
        // Note: blindly walking the connection pool and pushing anything we can find to the kill
        // queue does indeed mangle the kill order / timing. However,
        // 1. We are under no contract to provide a strict termination timeout; and
        // 2. Desynchronization of the kill queue should really not happen that often in practice.
        for (key, conn) in conn_map.iter() {
            if conn.is_shutting_down() {
                if self.is_full() {
                    return false;
                }
                self.push(*key);
            }
        }
        self.synced = true;
        true
    }

    /// Check if the kill queue is synchronized with the connection pool.
    ///
    pub fn is_synced(&self) -> bool {
        self.synced
    }

    /// Check if the kill queue is empty, obviously.
    ///
    pub fn is_empty(&self) -> bool {
        self.q.len() == 0
    }

    /// Check if the kill queue is full.
    ///
    pub fn is_full(&self) -> bool {
        self.q.len() == Self::SIZE
    }
}
