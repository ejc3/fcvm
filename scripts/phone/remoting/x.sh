#!/bin/bash
# x.sh <cmd...>: runs a command in the phone container
exec ~/fcvm-bench/target/release/fcvm exec --pid "$(cat /mnt/fcvm-btrfs/bench/vm.pid)" -- "$@"
