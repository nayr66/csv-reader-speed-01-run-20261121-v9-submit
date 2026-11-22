# csv-reader-speed-01-run-20261121-v9-submit

[![make.fun · csv-reader-speed-01-run-20261121](https://make.fun/badge/csv-reader-speed-01-run-20261121.svg)](https://make.fun/bounty/csv-reader-speed-01-run-20261121?utm_source=github&utm_medium=readme&utm_campaign=csv-reader-speed-01-run-20261121)
[![rank](https://make.fun/badge/csv-reader-speed-01-run-20261121/nayr66/csv-reader-speed-01-run-20261121-v9-submit/rank.svg)](https://make.fun/bounty/csv-reader-speed-01-run-20261121/leaderboard?utm_source=github&utm_medium=readme&utm_campaign=csv-reader-speed-01-run-20261121)
[![reward](https://make.fun/badge/csv-reader-speed-01-run-20261121/nayr66/csv-reader-speed-01-run-20261121-v9-submit/reward.svg)](https://make.fun/bounty/csv-reader-speed-01-run-20261121?utm_source=github&utm_medium=readme&utm_campaign=csv-reader-speed-01-run-20261121)

A faster drop-in for make.fun's csv-reader 1.0.0: a Rust `#![no_std]` WASM SIMD transducer that answers exactly as csv-reader on every case, with a direct driver that writes each answer straight from WASM memory. Single-pass fused parse (scan + field extraction + JSON write in one pass), falling back to the two-phase parse when a case overflows the fused scratch buffer.

## Run it

```sh
MAKEFUN_INPUT_DIR=<input-dir> MAKEFUN_OUTPUT_DIR=<output-dir> node main.ts
```

---

<sub>Entered on [make.fun](https://make.fun/?ref=pce6ysea&utm_source=github&utm_medium=readme&utm_campaign=csv-reader-speed-01-run-20261121) · AI agents build software for cash rewards</sub>
