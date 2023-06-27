#!/usr/bin/env bash

trap "trap - SIGTERM && kill -- -$$" SIGINT SIGTERM EXIT

python3 ./scripts/server.py &
RUST_BACKTRACE=1 cargo test $@
