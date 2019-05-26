// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//

/// `VsockPacket` provides a thin wrapper over the buffers exchanged via virtio queues.
/// There are two components to a vsock packet, each using its own descriptor in a
/// virtio queue:
/// - the packet header; and
/// - the packet data/buffer.
/// There is a 1:1 relation between descriptor chains and packets: the first (chain head) holds
/// the header, and an optional second descriptor holds the data. The second descriptor is only
/// present for data packets (VSOCK_OP_RW).
///
/// `VsockPacket` wraps these two buffers and provides direct access to the data stored
/// in guest memory. This is done to avoid unnecessarily copying data from guest memory
/// to temporary buffers, before passing it on to the vsock backend.
///
use std::ops::{Deref, DerefMut};

use super::super::DescriptorChain;
use super::defs;
use super::{Result, VsockError};

/// The vsock packet header.
//
// NOTE: this needs to be marked as repr(C), so we can predict its exact layout in memory, since
// we'll be using guest-provided pointers for access. The Linux UAPI headers define this struct as
// packed, but, in this particular case, packing only eliminates 4 trailing padding bytes.
// Declaring this struct as packed would also reduce its alignment to 1, which gets the Rust compiler
// all fidgety. Little does it know, the guest driver already aligned the structure properly, so
// we don't need to worry about alignment.
// That said, we'll be going with only repr(C) (no packing), and hard-coding the struct size as
// `VSOCK_PKT_HDR_SIZE`, since, given this particular layout, the first `VSOCK_PKT_HDR_SIZE` bytes
// are the same in both the packed and unpacked layouts.
//
// All fields use the little-endian byte order. Since we're only thinly wrapping a pointer
// to where the guest driver stored the packet header, let's restrict this to little-endian
// targets.
#[cfg(target_endian = "little")]
#[derive(Clone, Copy, Debug, Default)]
#[repr(C)]
pub struct VsockPacketHdr {
    /// Source CID.
    pub src_cid: u64,

    /// Destination CID.
    pub dst_cid: u64,

    /// Source port.
    pub src_port: u32,

    /// Destination port.
    pub dst_port: u32,

    /// Data length (in bytes) - may be 0, if there is now data buffer.
    pub len: u32,

    /// Socket type. Currently, only connection-oriented streams are defined by the vsock protocol.
    pub type_: u16,

    /// Operation ID - one of the VSOCK_OP_* values; e.g.
    /// - VSOCK_OP_RW: a data packet;
    /// - VSOCK_OP_REQUEST: connection request;
    /// - VSOCK_OP_RST: forcefull connection termination;
    /// etc (see `super::defs::uapi` for the full list).
    pub op: u16,

    /// Additional options (flags) associated with the current operation (`op`).
    /// Currently, only used with shutdown requests (VSOCK_OP_SHUTDOWN).
    pub flags: u32,

    /// Size (in bytes) of the packet sender receive buffer (for the connection to which this packet
    /// belongs).
    pub buf_alloc: u32,

    /// Number of bytes the sender has received and consumed (for the connection to which this packet
    /// belongs). For instance, for our Unix backend, this counter would be the total number of bytes
    /// we have successfully written to a backing Unix socket.
    pub fwd_cnt: u32,
}
/// The size (in bytes) of the above packet header struct, as present in a virtio queue buffer. See
/// the explanation above on why we are hard-coding this value here.
const VSOCK_PKT_HDR_SIZE: usize = 44;

/// A thin wrapper over a `VsockPacketHdr` pointer. This is useful because packet headers are provided
/// by the guest via virtio descriptors (so, basically, pointers). We never need to create header
/// structs - only access them.
/// Access to specific members of the wrapped struct is provided via `Deref` and `DerefMut` impls.
///
pub struct HdrWrapper {
    ptr: *mut VsockPacketHdr,
}

impl HdrWrapper {
    /// Create the wrapper from a virtio queue descriptor (a pointer), performing some sanity checks
    /// in the process.
    ///
    pub fn from_virtq_desc(desc: &DescriptorChain) -> Result<Self> {
        if desc.len < VSOCK_PKT_HDR_SIZE as u32 {
            return Err(VsockError::HdrDescTooSmall(desc.len));
        }

        desc.mem
            .checked_offset(desc.addr, VSOCK_PKT_HDR_SIZE)
            .ok_or(VsockError::GuestMemoryBounds)?;

        // It's safe to create the wrapper from this pointer, as:
        // - the guest driver aligned the data; and
        // - `GuestMemory` is page-aligned.
        //
        Ok(Self::from_ptr_unchecked(
            desc.mem
                .get_host_address(desc.addr)
                .map_err(VsockError::GuestMemory)?,
        ))
    }

    /// Create the wrapper from a raw pointer.
    ///
    /// Warning: the pointer needs to follow proper alignment for `VsockPacketHdr`. This is not a
    /// problem for virtq buffers, since the guest driver already handled alignment, and
    /// `GuestMemory` is page-aligned.
    ///
    fn from_ptr_unchecked(ptr: *const u8) -> Self {
        #[allow(clippy::cast_ptr_alignment)]
        Self {
            ptr: ptr as *mut VsockPacketHdr,
        }
    }

    /// Provide byte-wise access to the data stored inside the header, via a slice / fat-pointer.
    ///
    pub fn as_slice(&self) -> &[u8] {
        // This is safe, since `Self::from_virtq_head()` already performed all the bound checks.
        //
        unsafe { std::slice::from_raw_parts(self.ptr as *const u8, VSOCK_PKT_HDR_SIZE) }
    }

    /// Provide byte-wise mutable access to the data stored inside the header, via a slice / fat-pointer.
    ///
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // This is safe, since `Self::from_virtq_head()` already performed all the bound checks.
        //
        unsafe { std::slice::from_raw_parts_mut(self.ptr as *mut u8, VSOCK_PKT_HDR_SIZE) }
    }
}

/// `Deref` implementation for `HdrWrapper`, allowing access to `VsockPacketHdr` individual members.
///
impl Deref for HdrWrapper {
    type Target = VsockPacketHdr;

    fn deref(&self) -> &VsockPacketHdr {
        // Dereferencing this pointer is safe, because it was already validated by the `HdrWrapper`
        // constructor.
        unsafe { &*self.ptr }
    }
}

/// `DerefMut` implementation for `HdrWrapper`, allowing mutable access to `VsockPacketHdr`
/// individual members.
///
impl DerefMut for HdrWrapper {
    fn deref_mut(&mut self) -> &mut VsockPacketHdr {
        // Dereferencing this pointer is safe, because it was already validated by the `HdrWrapper`
        // constructor.
        unsafe { &mut *self.ptr }
    }
}

/// A thin wrapper over a vsock data pointer in guest memory. The wrapper is meant to be constructed
/// from a guest-provided virtq descriptor, and provides byte-slice-like access.
///
pub struct BufWrapper {
    ptr: *mut u8,
    len: usize,
}

impl BufWrapper {
    /// Create the data wrapper from a virtq descriptor.
    ///
    pub fn from_virtq_desc(desc: &DescriptorChain) -> Result<Self> {
        // Check the guest provided pointer and data size.
        desc.mem
            .checked_offset(desc.addr, desc.len as usize)
            .ok_or(VsockError::GuestMemoryBounds)?;

        Ok(Self::from_fat_ptr_unchecked(
            desc.mem
                .get_host_address(desc.addr)
                .map_err(VsockError::GuestMemory)?,
            desc.len as usize,
        ))
    }

    /// Create the data wrapper from a pointer and size.
    ///
    /// Warning: Both `ptr` and `len` must be insured as valid by the caller.
    ///
    fn from_fat_ptr_unchecked(ptr: *const u8, len: usize) -> Self {
        Self {
            ptr: ptr as *mut u8,
            len,
        }
    }

    /// Provide access to the data buffer, as a byte slice.
    ///
    pub fn as_slice(&self) -> &[u8] {
        // This is safe since bound checks have already been performed when creating the buffer
        // from the virtq descriptor.
        unsafe { std::slice::from_raw_parts(self.ptr as *const u8, self.len) }
    }

    /// Provide mutable access to the data buffer, as a byte slice.
    ///
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // This is safe since bound checks have already been performed when creating the buffer
        // from the virtq descriptor.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

/// The vsock packet, implemented as a wrapper over a virtq descriptor chain:
/// - the chain head, holding the packet header; and
/// - (an optional) data/buffer descriptor, only present for data packets (VSOCK_OP_RW).
///
pub struct VsockPacket {
    pub hdr: HdrWrapper,
    pub buf: Option<BufWrapper>,
}

impl VsockPacket {
    /// Create the packet wrapper from a TX virtq chain head.
    ///
    /// The chain head is expected to hold valid packet header data. A following packet buffer
    /// descriptor can optionally end the chain. Bounds and pointer checks are performed when
    /// creating the wrapper.
    ///
    pub fn from_tx_virtq_head(head: &DescriptorChain) -> Result<Self> {
        // All buffers in the TX queue must be readable.
        //
        if head.is_write_only() {
            return Err(VsockError::UnreadableDescriptor);
        }

        let hdr = HdrWrapper::from_virtq_desc(head)?;

        // Reject weirdly-sized packets.
        //
        if hdr.len > defs::MAX_PKT_BUF_SIZE as u32 {
            return Err(VsockError::InvalidPktLen(hdr.len));
        }

        // Don't bother to look for the data descriptor, if the header says there's no data.
        //
        if hdr.len == 0 {
            return Ok(Self { hdr, buf: None });
        }

        let buf_desc = head.next_descriptor().ok_or(VsockError::BufDescMissing)?;

        // All TX buffers must be readable.
        //
        if buf_desc.is_write_only() {
            return Err(VsockError::UnreadableDescriptor);
        }

        // The data descriptor should be large enough to hold the data length indicated by the header.
        //
        if buf_desc.len < hdr.len {
            return Err(VsockError::BufDescTooSmall);
        }

        Ok(Self {
            hdr,
            buf: Some(BufWrapper::from_virtq_desc(&buf_desc)?),
        })
    }

    /// Create the packet wrapper from an RX virtq chain head.
    ///
    /// There must be two descriptors in the chain, both writable: a header descriptor and a data
    /// descriptor. Bounds and pointer checks are performed when creating the wrapper.
    ///
    pub fn from_rx_virtq_head(head: &DescriptorChain) -> Result<Self> {
        // All RX buffers must be writable.
        //
        if !head.is_write_only() {
            return Err(VsockError::UnwritableDescriptor);
        }

        let hdr = HdrWrapper::from_virtq_desc(head)?;

        let buf_desc = head.next_descriptor().ok_or(VsockError::BufDescMissing)?;
        if !buf_desc.is_write_only() {
            return Err(VsockError::UnwritableDescriptor);
        }

        Ok(Self {
            hdr,
            buf: Some(BufWrapper::from_virtq_desc(&buf_desc)?),
        })
    }

    /// Builder-like method for setting `hdr.len` (the packet data length).
    ///
    pub fn set_len(&mut self, len: u32) -> &mut Self {
        self.hdr.len = len;
        self
    }

    /// Builder-like method for setting the `hdr.op` (the packet operation type).
    ///
    pub fn set_op(&mut self, op: u16) -> &mut Self {
        self.hdr.op = op;
        self
    }

    /// Builder-like method for setting a header flag.
    ///
    pub fn set_flag(&mut self, flag: u32) -> &mut Self {
        self.hdr.flags |= flag;
        self
    }
}
