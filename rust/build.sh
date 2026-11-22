#!/bin/bash
# Build the Rust WASM on the VPS (VM has no Rust), copy the .wasm back.
# Usage: bash build.sh   (run from the v9 dir on the VM)
set -e
V9=~/workspace/makefun/csv-reader-speed-01-run-20261121-v9
~/ssh-vps.sh "mkdir -p /home/ubuntu/csv-v9" >/dev/null 2>&1; ~/scp-vps.sh $V9/rust/lib.rs ubuntu@150.109.14.94:/home/ubuntu/csv-v9/
~/ssh-vps.sh "mkdir -p /home/ubuntu/csv-v9 && ~/.cargo/bin/rustc --edition 2021 --crate-type cdylib --target wasm32-unknown-unknown -C opt-level=3 -C debuginfo=0 -C target-feature=+simd128 -C panic=abort -C strip=debuginfo -C strip=symbols --out-dir /home/ubuntu/csv-v9 /home/ubuntu/csv-v9/lib.rs 2>&1 | tail -20; ls -la /home/ubuntu/csv-v9/*.wasm"
~/scp-vps.sh ubuntu@150.109.14.94:/home/ubuntu/csv-v9/lib.wasm $V9/rust/
ls -la $V9/rust/lib.wasm
