// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//

use std::ops::{Deref, DerefMut};

use super::super::DescriptorChain;
use super::{Result, VsockError};
use super::defs;


#[cfg(target_endian = "little")]
#[derive(Clone, Copy, Debug, Default)]
#[repr(C)]
pub struct VsockPacketHdr {
    pub src_cid: u64,
    pub dst_cid: u64,
    pub src_port: u32,
    pub dst_port: u32,
    pub len: u32,
    pub type_: u16,
    pub op: u16,
    pub flags: u32,
    pub buf_alloc: u32,
    pub fwd_cnt: u32,
    // pub _pad: u32,
}
const VSOCK_PKT_HDR_SIZE: usize = 44;

pub struct HdrWrapper {
    ptr: *mut VsockPacketHdr,
}
impl HdrWrapper {
    pub fn from_virtq_desc(desc: &DescriptorChain) -> Result<Self> {
        if desc.len < VSOCK_PKT_HDR_SIZE as u32 {
            return Err(VsockError::HdrDescTooSmall(desc.len));
        }
        desc.mem.checked_offset(desc.addr, VSOCK_PKT_HDR_SIZE)
            .ok_or(VsockError::GuestMemoryBounds)?;
        Ok(Self::from_ptr(
            desc.mem.get_host_address(desc.addr)
                .map_err(VsockError::GuestMemory)?
        ))
    }
    fn from_ptr(ptr: *const u8) -> Self {
        Self { ptr: ptr as *mut VsockPacketHdr }
    }
    pub fn as_slice(&self) -> &[u8] {
        unsafe {
            std::slice::from_raw_parts(
                self.ptr as *const u8,
                VSOCK_PKT_HDR_SIZE
            )
        }
    }
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe {
            std::slice::from_raw_parts_mut(
                self.ptr as *mut u8,
                VSOCK_PKT_HDR_SIZE
            )
        }
    }
}
impl Deref for HdrWrapper {
    type Target = VsockPacketHdr;
    fn deref(&self) -> &VsockPacketHdr {
        unsafe { &*self.ptr }
    }
}
impl DerefMut for HdrWrapper {
    fn deref_mut(&mut self) -> &mut VsockPacketHdr {
        unsafe { &mut *self.ptr }
    }
}

pub struct BufWrapper {
    ptr: *mut u8,
    len: usize,
}

impl BufWrapper {

    pub fn from_virtq_desc(desc: &DescriptorChain) -> Result<Self> {
        desc.mem.checked_offset(desc.addr, desc.len as usize)
            .ok_or(VsockError::GuestMemoryBounds)?;

        Ok(Self::from_fat_ptr(
            desc.mem.get_host_address(desc.addr).map_err(VsockError::GuestMemory)?,
            desc.len as usize
        ))
    }

    pub fn from_fat_ptr(ptr: *const u8, len: usize) -> Self {
        Self {
            ptr: ptr as *mut u8,
            len
        }
    }

    pub fn as_slice(&self) -> &[u8] {
        unsafe {
            std::slice::from_raw_parts(
                self.ptr as *const u8,
                self.len
            )
        }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe {
            std::slice::from_raw_parts_mut(
                self.ptr, self.len
            )
        }
    }

    pub fn len(&self) -> usize {
        self.as_slice().len()
    }
}


pub struct VsockPacket {
    pub hdr: HdrWrapper,
    pub buf: Option<BufWrapper>,
}


impl VsockPacket {

    pub fn from_tx_virtq_head(head: &DescriptorChain) -> Result<Self> {

        if head.is_write_only() {
            return Err(VsockError::UnreadableDescriptor);
        }

        let hdr = HdrWrapper::from_virtq_desc(head)?;
        if hdr.len > defs::MAX_PKT_BUF_SIZE as u32 {
            return Err(VsockError::InvalidPktLen(hdr.len));
        }

        if hdr.len == 0 {
            return Ok(Self { hdr, buf: None });
        }

        let buf_desc = head.next_descriptor()
            .ok_or(VsockError::BufDescMissing)?;

        if buf_desc.is_write_only() {
            return Err(VsockError::UnreadableDescriptor);
        }

        Ok(Self {
            hdr: hdr,
            buf: Some(BufWrapper::from_virtq_desc(&buf_desc)?),
        })
    }

    pub fn from_rx_virtq_head(head: &DescriptorChain) -> Result<Self> {
        if !head.is_write_only() {
            return Err(VsockError::UnwritableDescriptor);
        }

        let hdr = HdrWrapper::from_virtq_desc(head)?;

        let buf_desc = head.next_descriptor()
            .ok_or(VsockError::BufDescMissing)?;
        if !buf_desc.is_write_only() {
            return Err(VsockError::UnwritableDescriptor);
        }

        Ok(Self {
            hdr,
            buf: Some(BufWrapper::from_virtq_desc(&buf_desc)?),
        })

    }

    pub fn set_len(&mut self, len: u32) -> &mut Self {
        self.hdr.len = len;
        self
    }

    pub fn set_op(&mut self, op: u16) -> &mut Self {
        self.hdr.op = op;
        self
    }

    pub fn set_flags(&mut self, flags: u32) -> &mut Self {
        self.hdr.flags = flags;
        self
    }

    pub fn set_flag(&mut self, flag: u32) -> &mut Self {
        self.hdr.flags |= flag;
        self
    }
}


