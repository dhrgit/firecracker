// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

/// This module implements the Unix domain sockets backend for vsock. Guest vsock connections in the
/// guest are mapped to unix socket connections in the host. This requires a full implementation
/// of the vsock protocol, including connection multiplexing / state handling, and flow control.
///
/// From the guest point of view, this mapping is completely transparent - guest software uses
/// AF_VSOCK sockets, as it would with regular vhost/vsock. On the host side, there are two distinct
/// cases to be considered: host-initiated connections, and guest-initiated connections:
/// - For guest initiated connections, this vsock backend will forward the connection request to a
///   Unix domain socket, expected to be listening at a configurable path. Since the vsock protocol
///   uses port numbers to multiplex connections, and Unix sockets use filesystem paths, there will
///   have to be one path per (listening) vsock port. E.g. when guest software issue an AF_VSOCK
///   connection request to host port 52, this backend will attempt to establish a connection to an
///   AF_UNIX socket, expected to be listening at `/path/to/host_unix_socket_52`.
/// - For host initiated connections, this backend will be listening (on the host side), on a Unix
///   socket (e.g. at path `/path/to/host_unix_socket`). Host software will connect to this socket
///   and issue a connection forwarding command to the vsock guest port it is targeting. It will then
///   receive a connection confirmation, and may proceed to exchange stream data with the guest
///   software listening on the target port. In short, when connecting to guest port 52, the host will:
///   - connect to AF_UNIX `path/to/host_unix_socket`; then
///   - write "CONNECT 52<EOL>" to the AF_UNIX socket; then
///   - read "OK <assigned host vsock port><EOL>" from the AF_UNIX socket; then
///   - proceed to exchange data with the guest.
///
/// There are two main components to this module:
/// - the connection multiplexer - `muxer::VsockMuxer` - the entry point to this module, implementing
///   the `VsockBackend` trait; and
/// - the connection state handler - `connection::VsockConnection`.
/// Additionally, helper objects are used for:
/// - queuing guest-targeted vsock packets - `MuxerRxQ`;
/// - queuing connections scheduled for termination - `MuxerKillQ`;
/// - buffering TX (guest -> host) data - `TxBuf`.
///   Note that RX buffers aren't needed, as data is only read (i.e. transferred from host to guest)
///   after the guest has made a virtio RX buffer available. However, as a vsock device, it is our
///   responsibility to always process the virtio TX queue, so we may have to buffer TX data, if the
///   host isn't able to read it as fast as the guest driver makes TX buffers available. Once the guest
///   makes a TX buffer available, we have to evacuate it from the virtio TX queue, wether or not there's
///   a reader waiting for it on the host side.
///
mod connection;
pub mod muxer;
mod muxer_killq;
mod muxer_rxq;
mod txbuf;

mod defs {
    /// Maximum number of established connections that we can handle.
    pub const MAX_CONNECTIONS: usize = 1023;

    /// TX (guest -> host) buffer size, in bytes, per connection.
    pub const CONN_TX_BUF_SIZE: usize = 256 * 1024;

    /// After the guest thinks it has filled our TX buffer up this limit (in bytes), we'll send them
    /// a credit update packet, to let them know we can handle more.
    pub const CONN_CREDIT_UPDATE_THRESHOLD: usize = CONN_TX_BUF_SIZE - 64 * 1024;

    /// Maximum time (in milliseconds) between a clean connection shutdown request (VSOCK_OP_SHUTDOWN)
    /// and forcefull termination (VSOCK_OP_RST).
    pub const CONN_SHUTDOWN_TIMEOUT_MS: u64 = 2000;

    /// Size of the muxer RX packet queue.
    pub const MUXER_RXQ_SIZE: usize = 256;

    /// Size of the muxer connection kill queue.
    pub const MUXER_KILLQ_SIZE: usize = 128;
}

#[derive(Debug)]
pub enum Error {
    /// Attempted to write to a full buffer.
    BufferFull,
    /// Chained std::io::Error
    IoError(std::io::Error),
    /// The host made an invalid vsock port connection request.
    InvalidPortRequest,
    /// Attempted to push to a full queue.
    QueueFull,
    /// Muxer connection limit reached.
    TooManyConnections,
}
type Result<T> = std::result::Result<T, Error>;
