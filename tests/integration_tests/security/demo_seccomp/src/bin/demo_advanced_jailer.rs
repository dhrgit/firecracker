// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

extern crate libc;
extern crate seccomp;

mod seccomp_rules;

use std::env::args;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

use seccomp::{
    SeccompAction, SeccompCmpOp, SeccompCondition, SeccompFilter, SeccompRule,
    allow_syscall_if
};
use seccomp_rules::*;

fn main() {
    let args: Vec<String> = args().collect();
    let exec_file = &args[1];

    // Adds required rules.
    let mut all_rules = rust_required_rules();
    all_rules.extend(jailer_required_rules());

    // Adds rule to allow the harmless demo Firecracker.
    all_rules.push(allow_syscall_if(
        libc::SYS_write,
        vec![SeccompRule::new(
            vec![
                SeccompCondition::new(0, SeccompCmpOp::Eq, libc::STDOUT_FILENO as u64).unwrap(),
                SeccompCondition::new(2, SeccompCmpOp::Eq, 14).unwrap(),
            ],
            SeccompAction::Allow,
        )],
    ));

    let filter = SeccompFilter::new(all_rules, SeccompAction::Trap).unwrap();
    // Loads filters.
    filter.apply().unwrap();

    Command::new(exec_file)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .exec();
}
