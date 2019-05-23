// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use super::connection::VsockConnection;
use super::defs as unix_defs;
use super::muxer::{ConnMapKey, VsockMuxer};
use super::{Error, Result};

#[derive(Clone, Copy, Debug)]
struct MuxerKillQItem {
    key: ConnMapKey,
    kill_time: Instant,
}

pub struct MuxerKillQ {
    q: VecDeque<MuxerKillQItem>,
    synced: bool,
}

impl MuxerKillQ {
    pub fn new() -> Self {
        Self {
            q: VecDeque::with_capacity(unix_defs::MUXER_KILLQ_SIZE),
            synced: true,
        }
    }

    pub fn push(&mut self, key: ConnMapKey) -> Result<()> {
        if self.q.len() < unix_defs::MUXER_KILLQ_SIZE {
            self.q.push_back(MuxerKillQItem {
                key,
                kill_time: Instant::now()
                    + Duration::from_millis(unix_defs::CONN_SHUTDOWN_TIMEOUT_MS),
            });
            return Ok(());
        }
        self.synced = false;
        Err(Error::QueueFull)
    }

    pub fn pop(&mut self) -> Option<ConnMapKey> {
        if let Some(item) = self.q.front() {
            if Instant::now() > item.kill_time {
                return Some(self.q.pop_front().unwrap().key);
            }
        }
        None
    }

    pub fn sync(&mut self, conn_map: &mut HashMap<ConnMapKey, VsockConnection>) -> Result<()> {
        for (key, conn) in conn_map.iter_mut() {
            if conn.is_shutting_down() && !conn.get_muxer_flag(VsockMuxer::CONN_F_KILLQ) {
                self.push(*key)?;
            }
        }
        self.synced = true;
        Ok(())
    }

    pub fn is_synced(&self) -> bool {
        self.synced
    }
}
