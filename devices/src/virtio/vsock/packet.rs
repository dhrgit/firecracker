// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//


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
    pub _pad: u32,
}
impl VsockPacketHdr {
    const SIZE: usize = 44;

    pub fn from_virtq_desc(desc: &DescriptorChain) -> Result<Self> {
        if desc.is_write_only() {
            return Err(VsockError::UnreadableDescriptor);
        }
        if (desc.len as usize) < Self::SIZE {
            return Err(VsockError::HdrDescTooSmall(desc.len));
        }

        let hdr = VsockPacketHdr::default();
        desc.mem.read_slice_at_addr(
            unsafe {
                std::slice::from_raw_parts_mut(
                    &hdr as *const _ as *mut u8,
                    Self::SIZE
                )
            },
            desc.addr
        ).map_err(VsockError::GuestMemory)?;

        Ok(hdr)
    }

    pub fn write_to_virtq_desc(&self, desc: &DescriptorChain) -> Result<usize> {
        if !desc.is_write_only() {
            return Err(VsockError::UnwritableDescriptor);
        }
        if (desc.len as usize) < Self::SIZE {
            return Err(VsockError::HdrDescTooSmall(desc.len));
        }

        desc.mem.write_slice_at_addr(
            unsafe {
                std::slice::from_raw_parts(
                    self as *const _ as *const u8,
                    Self::SIZE
                )
            },
            desc.addr
        ).map_err(VsockError::GuestMemory)
    }

    pub fn with_len(mut self, len: u32) -> Self {
        self.len = len;
        self
    }

    pub fn with_flags(mut self, flags: u32) -> Self {
        self.flags |= flags;
        self
    }

}

pub struct VsockPacketBuf {
    ptr: *mut u8,
    len: usize,
}

impl VsockPacketBuf {

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
    pub hdr: VsockPacketHdr,
    pub buf: Option<VsockPacketBuf>,
}


impl VsockPacket {

    pub fn from_virtq_head(head: &DescriptorChain) -> Result<Self> {

        let hdr = VsockPacketHdr::from_virtq_desc(head)?;
        if (hdr.len as usize) > defs::MAX_PKT_BUF_SIZE {
            return Err(VsockError::InvalidPktLen(hdr.len));
        }

        let maybe_buf = match head.next_descriptor() {
            Some(buf_desc) => {
                if buf_desc.is_write_only() {
                    return Err(VsockError::UnreadableDescriptor);
                }
                if buf_desc.len < hdr.len {
                    return Err(VsockError::BufDescTooSmall);
                }
                Some(VsockPacketBuf::from_virtq_desc(&buf_desc)?)
            }
            None => None
        };

        Ok(Self { hdr, buf: maybe_buf })
    }

}


