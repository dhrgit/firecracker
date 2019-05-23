// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//

use std::collections::{HashMap, VecDeque};

use super::super::VsockChannel;
use super::connection::VsockConnection;
use super::defs as unix_defs;
use super::muxer::{ConnMapKey, MuxerRx, VsockMuxer};
use super::{Error, Result};

pub struct MuxerRxQ {
    q: VecDeque<MuxerRx>,
    synced: bool,
}

impl MuxerRxQ {
    pub fn new() -> Self {
        Self {
            q: VecDeque::with_capacity(unix_defs::MUXER_RXQ_SIZE),
            synced: true,
        }
    }

    pub fn push(&mut self, rx: MuxerRx) -> Result<()> {
        if self.q.len() < unix_defs::MUXER_RXQ_SIZE {
            self.q.push_back(rx);
            return Ok(());
        }

        if let MuxerRx::ConnRx(_) = rx {
            self.synced = false;
        }

        // TODO: if this is an RstPkt, we should try evicting any ConnRx before giving up

        Err(Error::QueueFull)
    }

    pub fn pop(&mut self) -> Option<MuxerRx> {
        self.q.pop_front()
    }

    pub fn sync(&mut self, conn_map: &mut HashMap<ConnMapKey, VsockConnection>) -> Result<()> {
        for (key, conn) in conn_map.iter_mut() {
            if conn.has_pending_rx() && !conn.get_muxer_flag(VsockMuxer::CONN_F_RXQ) {
                self.push(MuxerRx::ConnRx(*key))?;
                conn.set_muxer_flag(VsockMuxer::CONN_F_RXQ);
            }
        }
        self.synced = true;
        Ok(())
    }

    pub fn is_synced(&self) -> bool {
        self.synced
    }

    pub fn len(&self) -> usize {
        self.q.len()
    }
}
