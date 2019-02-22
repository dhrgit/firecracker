// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

extern crate libc;
extern crate seccomp;

mod seccomp_rules;

use std::env::args;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

use seccomp::{setup_seccomp, SeccompAction, SeccompFilterContext};
use seccomp_rules::*;

fn main() {
    let args: Vec<String> = args().collect();
    let exec_file = &args[1];

    let mut context =
        SeccompFilterContext::new(vec![].into_iter().collect(), SeccompAction::Trap).unwrap();

    // Adds required rules.
    let mut all_rules = rust_required_rules();
    all_rules.extend(jailer_required_rules());
    all_rules
        .into_iter()
        .try_for_each(|(syscall_number, (priority, rules))| {
            context.add_rules(syscall_number, priority, rules)
        })
        .unwrap();

    // Loads filters generated by the context.
    setup_seccomp(context).unwrap();

    Command::new(exec_file)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .exec();
}
