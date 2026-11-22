// csv-reader-speed-01 v9 — DIRECT driver (ALTERNATIVE, not the repo default).
// Same WASM as solve.js, but the answers.json bytes are written straight
// from the WASM output region: no JSON.parse per case and no JSON.stringify
// of the whole answer set. The house driver (main.ts) keeps run() +
// stringify; this driver exists because that pair alone costs ~25ms/case
// on the practice set, more than the WASM parse itself.
// Each case is parsed with the fused single-pass parse (runFusedAt) and its
// answer JSON is written to the file immediately, before the next case is
// parsed. Only one case's answer (~2.2MB max) ever sits in the WASM output
// region, so the process touches far fewer pages than buffering all answers
// (12MB) there first. runFusedAt falls back to the two-phase parse
// automatically if a case's long_row extra payloads overflow the fused
// scratch buffer.
// To use: replace main.ts with this file (same name, same repo) and verify
// with grade-local + kit check before any submit decision.
import { readFileSync, openSync, writeSync, closeSync } from 'node:fs';
import { join } from 'node:path';
import { runFusedAt, wasmMemU8, wasmOutPtr } from './solve.js';

const INPUT_DIR = process.env.MAKEFUN_INPUT_DIR ?? '/input';
const OUTPUT_DIR = process.env.MAKEFUN_OUTPUT_DIR ?? '/output';

const cases = JSON.parse(readFileSync(join(INPUT_DIR, 'cases.json'), 'utf8'));
const fd = openSync(join(OUTPUT_DIR, 'answers.json'), 'w');
const enc = new TextEncoder();
function w(s: string) {
  writeSync(fd, enc.encode(s));
}
w('[');
for (let i = 0; i < cases.length; i++) {
  if (i > 0) w(',');
  const outLen = runFusedAt(cases[i].input, 0);
  writeSync(fd, wasmMemU8.subarray(wasmOutPtr, wasmOutPtr + outLen));
}
w(']');
closeSync(fd);
