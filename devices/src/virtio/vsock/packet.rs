// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//


use std::cmp::min;

use memory_model::GuestMemory;

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
const VSOCK_PKT_HDR_SIZE: usize = 44;


#[derive(Default)]
pub struct VsockPacket {
    pub hdr: VsockPacketHdr,
    // TODO: maybe use an Option here?
    pub buf: Vec<u8>,
}

impl VsockPacket {

    pub fn from_virtq_head(head: &DescriptorChain, mem: &GuestMemory) -> Result<Self> {

        // TODO: maybe publish and use head.mem, instead of receving it as an arg here?

        if (head.len as usize) < VSOCK_PKT_HDR_SIZE {
            warn!("vsock: framing error, TX desc head too small for packet header");
            return Err(VsockError::PacketAssemblyError);
        }

        if head.is_write_only() {
            // TODO: return a proper error
            return Err(VsockError::GeneralError);
        }

        let hdr = VsockPacketHdr::default();
        mem.read_slice_at_addr(
            unsafe {
                std::slice::from_raw_parts_mut(
                    &hdr as *const _ as *mut u8,
                    VSOCK_PKT_HDR_SIZE
                )
            },
            head.addr
        ).map_err(|_| VsockError::PacketAssemblyError)?;

        if hdr.len > defs::MAX_PKT_BUF_SIZE as u32 {
            warn!("vsock: dropping TX packet with invalid len: {}", hdr.len);
            return Err(VsockError::PacketAssemblyError);
        }

        let mut buf = vec![0u8; hdr.len as usize];
        buf.resize(hdr.len as usize, 0);

        let mut read_cnt = 0usize;
        let mut maybe_desc = head.next_descriptor();
        while let Some(desc) = maybe_desc {
            if desc.is_write_only() {
                // TODO: return a proper error
                return Err(VsockError::GeneralError);
            }
            if read_cnt + (desc.len as usize) > (hdr.len as usize) {
                info!("vsock: malformed TX packet, vring data > hdr.len");
                return Err(VsockError::PacketAssemblyError);
            }
            mem.read_slice_at_addr(&mut buf[read_cnt..read_cnt + desc.len as usize], desc.addr)
                .map_err(|_| VsockError::PacketAssemblyError)?;
            read_cnt += desc.len as usize;
            maybe_desc = desc.next_descriptor();
        }

        if read_cnt != (hdr.len as usize) {
            info!("vsock: malformed TX packet, vring data != hdr.len");
            return Err(VsockError::PacketAssemblyError);
        }

        Ok(VsockPacket { hdr, buf })
    }

    pub fn write_to_virtq_head(&self, head: &DescriptorChain, mem: &GuestMemory) -> Result<usize> {

        if (head.len as usize) < VSOCK_PKT_HDR_SIZE {
            return Err(VsockError::GeneralError);
        }

        if !head.is_write_only() {
            return Err(VsockError::GeneralError);
        }

        // TODO: return a more descriptive error
        mem.write_slice_at_addr(
            unsafe {
                std::slice::from_raw_parts(
                    &self.hdr as *const _ as *const u8,
                    VSOCK_PKT_HDR_SIZE
                )
            },
            head.addr
        ).map_err(|_| VsockError::GeneralError)?;

        if self.hdr.len == 0 {
            return Ok(VSOCK_PKT_HDR_SIZE);
        }

        let mut write_cnt = 0usize;
        let mut maybe_desc = head.next_descriptor();
        while let Some(desc) = maybe_desc {
            if !desc.is_write_only() {
                return Err(VsockError::GeneralError);
            }
            let write_end = min(self.hdr.len as usize, write_cnt + desc.len as usize);
            write_cnt += mem
                .write_slice_at_addr(&self.buf[write_cnt..write_end], desc.addr)
                .map_err(|_| VsockError::GeneralError)?;
            maybe_desc = desc.next_descriptor();
        }

        // TODO: handle this error properly
        if write_cnt < self.hdr.len as usize {
            Err(VsockError::GeneralError)
        }
        else {
            Ok(VSOCK_PKT_HDR_SIZE + write_cnt)
        }
    }

}


