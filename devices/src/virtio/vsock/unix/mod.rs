// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

pub mod muxer;
mod connection;


const VSOCK_TX_BUF_SIZE: usize = 256*1024;

const TEMP_VSOCK_PATH: &str = "./vsock";
