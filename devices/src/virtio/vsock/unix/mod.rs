// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

pub mod muxer;
mod connection;
mod muxer_killq;
mod muxer_rxq;
mod txbuf;


mod defs {
    pub const MAX_CONNECTIONS: usize = 1023;
    pub const CONN_TX_BUF_SIZE: usize = 256 * 1024;
    pub const CONN_CREDIT_UPDATE_THRESHOLD: usize = CONN_TX_BUF_SIZE - 64 * 1024;
    pub const CONN_SHUTDOWN_TIMEOUT_MS: u64 = 3000;

    pub const MUXER_RXQ_SIZE: usize = 256;
    pub const MUXER_KILLQ_SIZE: usize = 128;
}


#[derive(Debug)]
pub enum Error {
    BrokenPipe,
    BufferFull,
    IoError(std::io::Error),
    ProtocolError,
    QueueFull,
    TooManyConnections,
}
type Result<T> = std::result::Result<T, Error>;
