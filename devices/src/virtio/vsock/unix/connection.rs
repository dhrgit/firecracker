// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//

/// `VsockConnection` covers the largest part of our implementation of the vsock protocol: the
/// connection state machine.
///
/// The `VsockConnection`'s main job is to forward data traffic, back and forth, between a guest-side
/// AF_VSOCK socket and a host-side AF_UNIX socket, while also managing its internal state. To that
/// end, `VsockConnection` implements:
/// - `VsockChannel` for:
///   - moving data from the host AF_UNIX socket to a guest-provied RX buffer, via `recv_pkt()`; and
///   - moving data from a guest-provided TX buffer to the host AF_UNIX socket, via `send_pkt()`; and
///   - updating its internal state, by absorbing control packets (anything other than VSOCK_OP_RW).
/// - `VsockEpollListener` for getting notified about the availablity of data or free buffer space at
///   the host AF_UNIX socket.
///
/// Note: there is a certain asymmetry to the RX and TX data flows:
///       - RX transfers do not need any data buffering, since data is read straight from the host
///         AF_UNIX socket and into the guest-provided RX buffer;
///       - TX transfers may require some data to be buffered by `VsockConnection`, if the host peer
///         can't keep up with reading the data that we're writing. This is because, once the guest
///         driver provides some data in a virtio TX buffer, the vsock device must consume it. If that
///         data can't be forwarded straight to the host socket, we'll have to store it in a buffer
///         (and flush it at a later time). Vsock flow control ensures that our TX buffer doesn't
///         overflow.
///
// The code in this file is best read with a fresh memory of the vsock protocol inner-workings.
// To help with that, here is a
//
// Short primer on the vsock protocol
// ----------------------------------
//
// 1. Establishing a connection
//    A vsock connection is considered established after a two-way handshake:
//    - the initiating peer sends a connection request packet (`hdr.op` == VSOCK_OP_REQUEST); then
//    - the listening peer sends back a connection response packet (`hdr.op` == VSOCK_OP_RESPONSE).
//
// 2. Terminating a connection
//    When a peer wants to shut down an established connection, it sends a VSOCK_OP_SHUTDOWN packet.
//    Two header flags are used with VSOCK_OP_SHUTDOWN, indicating the sender's intention:
//    - VSOCK_FLAGS_SHUTDOWN_RCV: the sender will receive no more data for this connection; and
//    - VSOCK_FLAGS_SHUTDOWN_SEND: the sender will send no more data for this connection.
//    After a shutdown packet, the receiving peer will have some protocol-undefined time to flush its
//    buffers, and then forcefully terminate the connection by sending back an RST packet. If the
//    shutdown-initiating peer doesn't receive this RST packet during a timeout period, it will send
//    one itself, thus terminating the connection.
//    Note: a peer can send more than one VSOCK_OP_SHUTDOWN packets. However, read/write indications
//          cannot be undone. E.g. once a "no-more-sending" promise was made, it cannot be taken back.
//          That is, `hdr.flags` will be ORed between subsequent VSOCK_OP_SHUTDOWN packets.
//
// 3. Flow control
//    Before sending a data packet (VSOCK_OP_RW), the sender must make sure that the receiver has
//    enough free buffer space to store that data. If this condition is not respected, the receiving
//    peer's behaviour is undefined. In this implementation, we forcefully terminate the connection
//    by sending back a VSOCK_OP_RST packet.
//    Note: all buffer space information is computed and store on a per-connection basis.
//    Peers keep each other informed about the free buffer space they have, by filling in two packet
//    header members with each packet they send:
//    - `hdr.buf_alloc`: the total buffer space the peer has allocated for receiving data; and
//    - `hdr.fwd_cnt`: the total number of bytes the peer has successfully flushed out of its buffer.
//    One can figure out how much space its peer has available in its buffer, by inspecting the
//    difference between how much it has sent to the peer and how much the peer has flushed out (i.e.
//    "forwarded", in the vsock spec terminology):
//    `peer_free = peer_buf_alloc - (total_bytes_sent_to_peer - peer_fwd_cnt)`.
//    Note: the above requires that peers constantly keep each other informed on their buffer space
//    situation. However, since there are no receipt acknowledgement packets defined for the vsock
//    protocol, packet flow can often be unidirectional (just one peer sending data to another), so
//    the sender's information about the receiver's buffer space can get quickly outdated. The vsock
//    protocol defines two solutions to this problem:
//    1. The sender can explicitly ask for a buffer space (i.e. "credit") update from its peer, via
//       a VSOCK_OP_CREDIT_REQUEST packet, to which it will get a VSOCK_OP_CREDIT_UPDATE response
//       (or any response will do, really, since credit information must be included in any packet);
//    2. The receiver can be proactive, and send VSOCK_OP_CREDIT_UPDATE packet, whenever it thinks
//       its peer's information is out of date.
//    Our implementation uses the proactive approach.
//
use std::io::{ErrorKind, Read, Write};
use std::num::Wrapping;
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;

use super::super::defs::uapi;
use super::super::packet::VsockPacket;
use super::super::{Result as VsockResult, VsockError};
use super::super::{VsockChannel, VsockEpollListener};
use super::txbuf::TxBuf;
use super::{defs as unix_defs, Error, Result};

/// A `VsockConnection` state.
///
#[derive(Debug, PartialEq)]
pub enum ConnState {
    /// The connection has been initiated by the host end, but is yet to be confirmed by the guest.
    LocalInit,
    /// The connection has been initiated by the guest, but we are yet to confirm it, by sending
    /// a response packet (VSOCK_OP_RESPONSE).
    PeerInit,
    /// The connection handshake has been performed succesfully, and data can now be exchanged.
    Established,
    /// The host (AF_UNIX) socket was closed.
    LocalClosed,
    /// A VSOCK_OP_SHUTDOWN packet was received from the guest. The tuple represents the guest R/W
    /// indication: (will_not_recv_anymore_data, will_not_send_anymore_data).
    PeerClosed(bool, bool),
    /// The connection is scheduled to be forcefully terminated as soon as possible.
    Killed,
}

/// An RX indication, used by `VsockConnection` to schedule future `recv_pkt()` responses.
/// For instance, after being notified that there is available data to be read from the AF_UNIX
/// socket (via `notify()`), the connection will store a `PendingRx::Rw` to be later inspected by
/// `recv_pkt()`.
///
#[derive(Clone, Copy, PartialEq)]
enum PendingRx {
    /// We need to yield a connection request packet (VSOCK_OP_REQUEST).
    Request = 0,
    /// We need to yield a connection response packet (VSOCK_OP_RESPONSE).
    Response = 1,
    /// We need to yeild a forcefull connection terminaton packet (VSOCK_OP_RST).
    Rst = 2,
    /// We need to yield a data packet (VSOCK_OP_RW), by reading from the AF_UNIX socket.
    Rw = 3,
    /// We need to yield a credit update packet (VSOCK_OP_CREDIT_UPDATE).
    CreditUpdate = 4,
}
impl PendingRx {
    /// Transform the enum value into a bitmask, that can be used for set operations.
    ///
    fn into_mask(self) -> u16 {
        1u16 << (self as u16)
    }
}

/// A set of RX indications (`PendingRx` items).
///
struct PendingRxSet {
    data: u16,
}

impl PendingRxSet {
    /// Insert an item into the set.
    ///
    fn insert(&mut self, it: PendingRx) {
        self.data |= it.into_mask();
    }

    /// Remove an item from the set and return:
    /// - true, if the item was in the set; or
    /// - false, if the item wasn't in the set.
    ///
    fn remove(&mut self, it: PendingRx) -> bool {
        let ret = self.contains(it);
        self.data &= !it.into_mask();
        ret
    }

    /// Check if an item is present in this set.
    ///
    fn contains(&self, it: PendingRx) -> bool {
        self.data & it.into_mask() != 0
    }

    /// Check if the set is empty.
    ///
    fn is_empty(&self) -> bool {
        self.data == 0
    }
}

/// Create a set containing only one item.
///
impl From<PendingRx> for PendingRxSet {
    fn from(it: PendingRx) -> Self {
        Self {
            data: it.into_mask(),
        }
    }
}

/// A self-managing connection object, that handles communication between a guest-side AF_VSOCK socket
/// and a host-side AF_UNIX socket.
///
pub struct VsockConnection {
    /// The current connection state.
    state: ConnState,
    /// The peer (guest) CID.
    /// Note: the local CID is always `uapi::VSOCK_HOST_CID`.
    peer_cid: u64,
    /// The local (host) port.
    local_port: u32,
    /// The peer (guest) port.
    peer_port: u32,
    /// The (connected) host-side AF_UNIX socket.
    stream: UnixStream,
    /// The TX buffer for this connection.
    tx_buf: TxBuf,
    /// Total number of bytes that have been successfully written to `self.stream`, either directly,
    /// or flushed from `self.tx_buf`.
    fwd_cnt: Wrapping<u32>,
    /// The amount of buffer space that the peer (guest) has allocated for this connection.
    peer_buf_alloc: u32,
    /// The total number of bytes that the peer has forwarded away.
    peer_fwd_cnt: Wrapping<u32>,
    /// The total number of bytes sent to the peer (guest vsock driver)
    rx_cnt: Wrapping<u32>,
    /// Our `self.fwd_cnt`, as last sent to the peer. This is used to provide proactive credit updates,
    /// and let the peer know it's OK to send more data.
    last_fwd_cnt_to_peer: Wrapping<u32>,
    /// The set of pending RX packet indications, that `recv_pkt()` will use to fill in a packet for
    /// the peer (guest).
    pending_rx: PendingRxSet,
}

impl VsockChannel for VsockConnection {
    /// Fill in a vsock packet, to be delivered to our peer (the guest driver).
    ///
    /// As per the `VsockChannel` trait, this should only be called when there is data to be fetched
    /// from the channel (i.e. `has_pending_rx()` is true). Otherwise, it will error out with
    /// `VsockError::NoData`.
    /// Pending RX indications are set by other mutable actions performed on the channel. For instance,
    /// `send_pkt()` could set an Rst indication, if called with a VSOCK_OP_SHUTDOWN packet, or
    /// `notify()` could set a Rw indication (a data packet can be fetched from the channel), if data
    /// was ready to be read from the host socket.
    ///
    /// Returns:
    /// - `Ok(())`: the packet has been successfully filled in and is ready for delivery;
    /// - `Err(VsockError::NoData)`: there was no data available with which to fill in the packet;
    /// - `Err(VsockError::PktBufMissing)`: the packet would've been filled in with data, but it is
    ///   missing the data buffer.
    ///
    fn recv_pkt(&mut self, pkt: &mut VsockPacket) -> VsockResult<()> {
        // Perform some generic initialization that is the same for any packet operation (e.g.
        // source, destination, credit, etc).
        self.init_pkt(pkt);

        // If forcefull termination is pending, there's no point in checking for anything else.
        // It's dead, Jim.
        if self.pending_rx.remove(PendingRx::Rst) {
            pkt.set_op(uapi::VSOCK_OP_RST);
            return Ok(());
        }

        // Next up: if we're due a connection confirmation, that's all we need to know to fill in
        // this packet.
        if self.pending_rx.remove(PendingRx::Response) {
            self.state = ConnState::Established;
            pkt.set_op(uapi::VSOCK_OP_RESPONSE);
            return Ok(());
        }

        // Same thing goes for locally-initiated connections, that need to yield a connection request.
        if self.pending_rx.remove(PendingRx::Request) {
            pkt.set_op(uapi::VSOCK_OP_REQUEST);
            return Ok(());
        }

        // A credit update is basically a no-op, so we should only waste a perfectly fine RX buffer
        // on it if we really have nothing else to say.
        if self.pending_rx.remove(PendingRx::CreditUpdate) && !self.has_pending_rx() {
            pkt.set_op(uapi::VSOCK_OP_CREDIT_UPDATE);
            self.last_fwd_cnt_to_peer = self.fwd_cnt;
            return Ok(());
        }

        // Alright, if we got to here, we need to cough up a data packet. We've already checked for
        // all other pending RX indications.
        self.pending_rx.remove(PendingRx::Rw);

        match self.state {
            // A data packet is only valid for established connections, and connections for which
            // our peer has initiated a graceful shutdown, but can still receive data.
            ConnState::Established | ConnState::PeerClosed(false, _) => (),
            _ => {
                // Any other connection state is invalid at this point, and we need to kill it with
                // fire.
                pkt.set_op(uapi::VSOCK_OP_RST);
                return Ok(());
            }
        }

        // Oh wait, before we start bringing in the big data, can our peer handle receiving so much
        // bytey goodness?
        if self.need_credit_update_from_peer() {
            self.last_fwd_cnt_to_peer = self.fwd_cnt;
            pkt.set_op(uapi::VSOCK_OP_CREDIT_REQUEST);
            return Ok(());
        }

        let buf = pkt.buf.as_mut().ok_or(VsockError::PktBufMissing)?;

        // The maximum amount of data we can read in is limited by both the RX buffer size and the
        // peer available buffer space.
        let max_len = std::cmp::min(buf.as_slice().len(), self.peer_avail_credit());

        // Read data from the socket straight to the RX buffer, for maximum throughput.
        match self.stream.read(&mut buf.as_mut_slice()[..max_len]) {
            Ok(read_cnt) => {
                if read_cnt == 0 {
                    // A 0-length read means the host socket was closed down. In that case, we'll ask
                    // our peer to shut down the connection. We can neither send nor receive any more
                    // data.
                    self.state = ConnState::LocalClosed;
                    pkt.set_op(uapi::VSOCK_OP_SHUTDOWN)
                        .set_flag(uapi::VSOCK_FLAGS_SHUTDOWN_RCV)
                        .set_flag(uapi::VSOCK_FLAGS_SHUTDOWN_SEND);
                } else {
                    // On a successful data read, we fill in the packet with the RW op, and length of
                    // the read data.
                    pkt.set_op(uapi::VSOCK_OP_RW).set_len(read_cnt as u32);
                }
            }
            Err(e) => {
                if e.kind() == std::io::ErrorKind::WouldBlock {
                    // If reading from the host socket would've blocked now, it might succeed later.
                    return Err(VsockError::NoData);
                }
                // We don't know how to handle any other I/O error, so it's time to kill this
                // connection.
                pkt.set_op(uapi::VSOCK_OP_RST);
            }
        };

        self.rx_cnt += Wrapping(pkt.hdr.len);
        self.last_fwd_cnt_to_peer = self.fwd_cnt;

        Ok(())
    }

    /// Deliver a guest-generated packet to this connection.
    ///
    /// This forwards the data in RW packets to the host AF_UNIX socket, and absorbs control packets,
    /// using the to manage the internal connection state.
    ///
    /// Returns:
    /// always `Ok(())`: the packet has been consumed;
    ///
    fn send_pkt(&mut self, pkt: &VsockPacket) -> VsockResult<()> {
        // Update the peer credit information.
        self.peer_buf_alloc = pkt.hdr.buf_alloc;
        self.peer_fwd_cnt = Wrapping(pkt.hdr.fwd_cnt);

        match self.state {
            // Most frequent case: this is an established connection that needs to forward some data
            // to the host socket. Also works for a connection that has begun shutting down, but the
            // peer still has some data to send.
            ConnState::Established | ConnState::PeerClosed(_, false)
                if pkt.hdr.op == uapi::VSOCK_OP_RW =>
            {
                if pkt.buf.is_none() {
                    info!(
                        "vsock: dropping empty data packet from guest (lp={}, pp={}",
                        self.local_port, self.peer_port
                    );
                    return Ok(());
                }

                // Unwrapping here is safe, since we just checked `pkt.buf` above.
                let buf_slice = &pkt.buf.as_ref().unwrap().as_slice()[..(pkt.hdr.len as usize)];
                if let Err(err) = self.send_bytes(buf_slice) {
                    // If we can't write to the host socket, that's an unrecoverable error, so we'll
                    // terminate this connection.
                    info!(
                        "vsock: error writing to local stream (lp={}, pp={}): {:?}",
                        self.local_port, self.peer_port, err
                    );
                    self.kill();
                    return Ok(());
                }

                // We might've just consumed some data. If that's the case, we might need to update
                // the peer on our buffer space situation, so that it can keep sending data packets
                // our way.
                if self.peer_needs_credit_update() {
                    self.pending_rx.insert(PendingRx::CreditUpdate);
                }
            }

            // Next up: receing a response / confirmation for a host-initiated connection. We'll
            // move to an Established state, and pass on the good news through the AF_UNIX socket.
            ConnState::LocalInit if pkt.hdr.op == uapi::VSOCK_OP_RESPONSE => {
                self.send_bytes(format!("OK {}\n", self.local_port).as_bytes())
                    .and_then(|_| {
                        self.state = ConnState::Established;
                        Ok(())
                    })
                    .unwrap_or_else(|_| {
                        self.kill();
                    });
            }

            // The peer wants to shut down an established connection.
            // If they have nothing more to send nor receive, and we don't have to wait to drain our
            // TX buffer, we can terminate the connection right away. Otherwise, the muxer will add
            // this connection to its kill queue, and terminate it once the shutdown timeout expires.
            ConnState::Established if pkt.hdr.op == uapi::VSOCK_OP_SHUTDOWN => {
                let recv_off = pkt.hdr.flags & uapi::VSOCK_FLAGS_SHUTDOWN_RCV != 0;
                let send_off = pkt.hdr.flags & uapi::VSOCK_FLAGS_SHUTDOWN_SEND != 0;
                self.state = ConnState::PeerClosed(recv_off, send_off);
                if recv_off && send_off && self.tx_buf.is_empty() {
                    self.pending_rx.insert(PendingRx::Rst);
                }
            }

            // The peer wants to update a shutdown request, with more receive/send indications.
            // The same logic as above applies.
            ConnState::PeerClosed(ref mut recv_off, ref mut send_off)
                if pkt.hdr.op == uapi::VSOCK_OP_SHUTDOWN =>
            {
                *recv_off = *recv_off || (pkt.hdr.flags & uapi::VSOCK_FLAGS_SHUTDOWN_RCV != 0);
                *send_off = *send_off || (pkt.hdr.flags & uapi::VSOCK_FLAGS_SHUTDOWN_SEND != 0);
                if *recv_off && *send_off && self.tx_buf.is_empty() {
                    self.pending_rx.insert(PendingRx::Rst);
                }
            }

            // A credit update from our peer is valid only in a state which allows data transfer
            // towards the peer.
            ConnState::Established | ConnState::PeerInit | ConnState::PeerClosed(false, _)
                if pkt.hdr.op == uapi::VSOCK_OP_CREDIT_UPDATE =>
            {
                // Nothing to do here; we've already updated peer credit.
            }

            // A credit request from our peer is valid only in a state which allows data transfer
            // from the peer. We'll respond with a credit update packet.
            ConnState::Established | ConnState::PeerInit | ConnState::PeerClosed(_, false)
                if pkt.hdr.op == uapi::VSOCK_OP_CREDIT_REQUEST =>
            {
                self.pending_rx.insert(PendingRx::CreditUpdate);
            }

            _ => {
                debug!(
                    "vsock: dropping invalid TX pkt for connection: state={:?}, pkt.hdr={:?}",
                    self.state, *pkt.hdr
                );
            }
        };

        Ok(())
    }

    /// Check if the connection has any pending packet addressed to the peer.
    ///
    fn has_pending_rx(&self) -> bool {
        !self.pending_rx.is_empty()
    }
}

impl VsockEpollListener for VsockConnection {
    /// Get the file descriptor that this connection wants polled.
    ///
    /// The connection is interested in being notified about EPOLLIN / EPOLLOUT events on the host
    /// AF_UNIX socket.
    ///
    fn get_polled_fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }

    /// Get the event set that this connection is interested in.
    ///
    /// A connection will want to be notified when:
    /// - data is available to be read from the AF_UNIX socket, so that it can store an RW pending
    ///   RX indication; and
    /// - data can be written to the AF_UNIX socket, and the TX buffer needs to be flushed.
    ///
    fn get_polled_evset(&self) -> epoll::Events {
        let mut evset = epoll::Events::empty();
        if !self.tx_buf.is_empty() {
            // There's data waiting in the TX buffer, so we are interested in being notified when
            // writing to the host socket wouldn't block.
            evset.insert(epoll::Events::EPOLLOUT);
        }
        // We're generally interested in being notified when data can be read from the host socket,
        // unless we're in a state which doesn't allow moving data from host to guest.
        match self.state {
            ConnState::LocalClosed | ConnState::PeerClosed(true, _) => (),
            _ if self.need_credit_update_from_peer() => (),
            _ => evset.insert(epoll::Events::EPOLLIN),
        }
        evset
    }

    /// Notify the connection about an event (or set of events) that it was interested in.
    ///
    fn notify(&mut self, evset: epoll::Events) {
        if evset.contains(epoll::Events::EPOLLIN) {
            // Data can be read from the host socket. Setting a Rw pending indication, so that the
            // muxer will know to call `recv_pkt()` later.
            self.pending_rx.insert(PendingRx::Rw);
        }

        if evset.contains(epoll::Events::EPOLLOUT) {
            // Data can be written to the host socket. Time to flush out the TX buffer.
            //
            if self.tx_buf.is_empty() {
                info!("vsock: connection received unexpected EPOLLOUT event");
                return;
            }
            let flushed = self
                .tx_buf
                .flush_to(&mut self.stream)
                .unwrap_or_else(|err| {
                    info!(
                        "vsock: error flushing TX buf for (lp={}, pp={}): {:?}",
                        self.local_port, self.peer_port, err
                    );
                    self.kill();
                    0
                });
            self.fwd_cnt += Wrapping(flushed as u32);

            // If we've freed up some more buffer space, we may need to let the peer know it can
            // safely send more data our way.
            if self.peer_needs_credit_update() {
                self.pending_rx.insert(PendingRx::CreditUpdate);
            }

            // If this connection was shutting down, but is wating to drain the TX buffer
            // before forcefull termination, the wait might be over.
            if self.tx_buf.is_empty() && self.is_shutting_down() {
                self.pending_rx.insert(PendingRx::Rst);
            }
        }
    }
}

impl VsockConnection {
    /// Create a new guest-initiated connection object.
    ///
    pub fn new_peer_init(
        stream: UnixStream,
        peer_cid: u64,
        local_port: u32,
        peer_port: u32,
        peer_buf_alloc: u32,
    ) -> Self {
        Self {
            peer_cid,
            local_port,
            peer_port,
            stream,
            state: ConnState::PeerInit,
            tx_buf: TxBuf::new(),
            fwd_cnt: Wrapping(0),
            peer_buf_alloc,
            peer_fwd_cnt: Wrapping(0),
            rx_cnt: Wrapping(0),
            last_fwd_cnt_to_peer: Wrapping(0),
            pending_rx: PendingRxSet::from(PendingRx::Response),
        }
    }

    /// Create a new host-initiated connection object.
    ///
    pub fn new_local_init(
        stream: UnixStream,
        peer_cid: u64,
        local_port: u32,
        peer_port: u32,
    ) -> Self {
        Self {
            peer_cid,
            local_port,
            peer_port,
            stream,
            state: ConnState::LocalInit,
            tx_buf: TxBuf::new(),
            fwd_cnt: Wrapping(0),
            peer_buf_alloc: 0,
            peer_fwd_cnt: Wrapping(0),
            rx_cnt: Wrapping(0),
            last_fwd_cnt_to_peer: Wrapping(0),
            pending_rx: PendingRxSet::from(PendingRx::Request),
        }
    }

    /// Check if this connection is currently shutting down, and should be scheduled for forcefull
    /// termination.
    ///
    pub fn is_shutting_down(&self) -> bool {
        // If there's already an RST pending, the connection is all but dead, so there's no need to
        // schedule it for later termination.
        !self.pending_rx.contains(PendingRx::Rst)
            && match self.state {
                // This connection should be scheduled for termination if:
                // - a full shutdown was requested by our peer (no more recv + no more send); or
                // - our host socket is gone (closed), in which case we had requested a full
                //   shutdown.
                ConnState::PeerClosed(true, true) | ConnState::LocalClosed => true,
                _ => false,
            }
    }

    /// Schedule the connection to be forcefully terminated ASAP (i.e. the next time the connection is
    /// asked to yield a packet, via `recv_pkt()`).
    ///
    pub fn kill(&mut self) {
        self.state = ConnState::Killed;
        self.pending_rx.insert(PendingRx::Rst);
    }

    /// Send some raw data (a byte-slice) to the host AF_UNIX socket.
    ///
    /// Raw data can either be sent straight to the unix socket, or to our TX buffer, if the former
    /// fails.
    ///
    fn send_bytes(&mut self, buf: &[u8]) -> Result<()> {
        if !self.tx_buf.is_empty() {
            // If there is data in our TX buffer, we'll need to try and flush it out first.
            // Flushing data out of the TX buffer also increases the "forwarded" bytes counter, since
            // it's freed up some space for us.
            self.fwd_cnt += Wrapping(self.tx_buf.flush_to(&mut self.stream)? as u32);
            if self.tx_buf.is_empty() {
                // Alright, we've drained the TX buffer; now, let's try this again, from the top.
                self.send_bytes(buf)?;
            } else {
                // Couldn't drain the TX buffer, so the new data needs to go on top of the buffer.
                self.tx_buf.push(buf)?;
            }
            return Ok(());
        }

        // The TX buffer is empty, so we can try to write straight to the socket.
        let written = match self.stream.write(buf) {
            Ok(cnt) => cnt,
            Err(e) => {
                // Absorb any would-block errors, since we can always try again later.
                if e.kind() == ErrorKind::WouldBlock {
                    0
                } else {
                    // We don't know how to handle any other write error, so we'll send it up the
                    // call chain.
                    return Err(Error::IoError(e));
                }
            }
        };
        // Move the "forwarded bytes" counter ahead by how much we were able to send out.
        self.fwd_cnt += Wrapping(written as u32);

        // If we couldn't write the whole slice, we'll need to push the remaining data to our buffer.
        if written < buf.len() {
            self.tx_buf.push(&buf[written..])?;
        }

        Ok(())
    }

    /// Check if the credit information the peer has last received from us is outdated.
    ///
    fn peer_needs_credit_update(&self) -> bool {
        (self.fwd_cnt - self.last_fwd_cnt_to_peer).0 as usize
            >= unix_defs::CONN_CREDIT_UPDATE_THRESHOLD
    }

    /// Check if we need to ask the peer for a credit update before sending any more data its way.
    ///
    fn need_credit_update_from_peer(&self) -> bool {
        self.peer_avail_credit() == 0
    }

    /// Get the maximum number of bytes that we can send to our peer, without overflowing its buffer.
    ///
    fn peer_avail_credit(&self) -> usize {
        (Wrapping(self.peer_buf_alloc as u32) - (self.rx_cnt - self.peer_fwd_cnt)).0 as usize
    }

    /// Prepare a packet header for transmission to our peer.
    ///
    fn init_pkt<'a>(&self, pkt: &'a mut VsockPacket) -> &'a mut VsockPacket {
        // Make sure the header is zeroed-out first.
        // This looks sub-optimal, but it is actually optimized-out in the compiled code to be faster
        // than a memset().
        for b in pkt.hdr.as_mut_slice() {
            *b = 0;
        }

        pkt.hdr.src_cid = uapi::VSOCK_HOST_CID;
        pkt.hdr.dst_cid = self.peer_cid;
        pkt.hdr.src_port = self.local_port;
        pkt.hdr.dst_port = self.peer_port;
        pkt.hdr.type_ = uapi::VSOCK_TYPE_STREAM;
        pkt.hdr.buf_alloc = unix_defs::CONN_TX_BUF_SIZE as u32;
        pkt.hdr.fwd_cnt = self.fwd_cnt.0;
        pkt
    }
}
