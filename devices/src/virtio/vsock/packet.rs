// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//


use std::ops::{Deref, DerefMut};

use memory_model::DataInit;
use virtio_gen::virtio_vsock::virtio_vsock_hdr;

use super::*;



// TODO: fix this struct, since packing implies align=1
#[derive(Clone, Copy, Debug, Default)]
#[repr(C, packed)]
pub struct VsockPacketHdr(virtio_vsock_hdr);

unsafe impl DataInit for VsockPacketHdr {}

impl Deref for VsockPacketHdr {
    type Target = virtio_vsock_hdr;
    fn deref(&self) -> &virtio_vsock_hdr {
        &self.0
    }
}

impl DerefMut for VsockPacketHdr {
    fn deref_mut(&mut self) -> &mut virtio_vsock_hdr {
        &mut self.0
    }
}


#[derive(Default)]
pub struct VsockPacket {
    pub hdr: VsockPacketHdr,
    pub buf: Vec<u8>,
}

impl VsockPacket {
    pub fn new_rst(src_cid: u64, dst_cid: u64, src_port: u32, dst_port: u32) -> Self {
        // TODO: set buf_alloc
        Self {
            hdr: VsockPacketHdr(virtio_vsock_hdr {
                src_cid,
                dst_cid,
                src_port,
                dst_port,
                type_: VSOCK_TYPE_STREAM,
                op: VSOCK_OP_RST,
                ..Default::default()
            }),
            buf: Vec::new(),
        }
    }

    pub fn new_response(src_cid: u64, dst_cid: u64, src_port: u32, dst_port: u32) -> Self {
        Self {
            hdr: VsockPacketHdr(virtio_vsock_hdr {
                src_cid,
                dst_cid,
                src_port,
                dst_port,
                type_: VSOCK_TYPE_STREAM,
                op: VSOCK_OP_RESPONSE,
                buf_alloc: 65536,
                ..Default::default()
            }),
            buf: Vec::new(),
        }
    }

    pub fn new_rw(src_cid: u64, dst_cid: u64, src_port: u32, dst_port: u32, buf: Vec<u8>) -> Self {
        Self {
            hdr: VsockPacketHdr(virtio_vsock_hdr {
                src_cid,
                dst_cid,
                src_port,
                dst_port,
                type_: VSOCK_TYPE_STREAM,
                op: VSOCK_OP_RW,
                len: buf.len() as u32,
                ..Default::default()
            }),
            buf,
        }
    }

}


