// csv-reader-speed-01 v9 — DIRECT driver (ALTERNATIVE, not the repo default).
// Same WASM as solve.js, but the answers.json bytes are written straight
// from the WASM output region: no JSON.parse per case and no JSON.stringify
// of the whole answer set. The house driver (main.ts) keeps run() +
// stringify; this driver exists because that pair alone costs ~25ms/case
// on the practice set, more than the WASM parse itself.
// The cases' JSON is laid out contiguously in the WASM output region with
// runRawAt (separators written straight into that region), then the whole
// slice is written once — no per-case copies.
// To use: replace main.ts with this file (same name, same repo) and verify
// with grade-local + kit check before any submit decision.
import { readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { runRawAt, wasmMemU8, wasmOutPtr } from './solve.js';

const INPUT_DIR = process.env.MAKEFUN_INPUT_DIR ?? '/input';
const OUTPUT_DIR = process.env.MAKEFUN_OUTPUT_DIR ?? '/output';

const cases = JSON.parse(readFileSync(join(INPUT_DIR, 'cases.json'), 'utf8'));
let off = 0;
wasmMemU8[wasmOutPtr + off++] = 0x5b; // [
for (let i = 0; i < cases.length; i++) {
  if (i > 0) wasmMemU8[wasmOutPtr + off++] = 0x2c; // ,
  off += runRawAt(cases[i].input, off);
}
wasmMemU8[wasmOutPtr + off++] = 0x5d; // ]
writeFileSync(
  join(OUTPUT_DIR, 'answers.json'),
  wasmMemU8.subarray(wasmOutPtr, wasmOutPtr + off),
);
