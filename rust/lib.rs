// csv-reader-speed-01 v9: Rust #![no_std] WASM transducer.
// Ports solve.js v8 (proven correct vs csv-reader 1.0.0) exactly, and writes
// the answer as UTF-8 JSON text into linear memory. JS glue (solve.js) then
// either JSON.parses it (house driver) or writes the bytes out directly.
// No allocator: everything lives in one static arena. SIMD (v128) for the
// delimiter scans and the ASCII bulk JSON copy. Hot paths use specialised
// scans (no runtime `want` branches) and hoisted splat constants.
//
// Memory layout: ONE static arena MEM (WASM memory is zero-initialised,
// so this costs address space, not .wasm bytes):
//   INPUT  [0, 8M)        : input as UTF-16 code units (4M units)
//   POOL   [8M, 24M)      : field values as UTF-16 units (8M units)
//   OUT    [24M, 56M)     : JSON output bytes (32M)
//   FIELDS [56M, 72M)     : (pool_off, len) per field (2M)
//   ROWS   [72M, 73.6M)   : (field_start, count) per row (200k)
//   ERRORS [73.6M, 75.6M) : buffered errors (100k x 20B)

#![no_std]

use core::arch::wasm32::*;
use core::panic::PanicInfo;

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    unreachable()
}

// ---------------------------------------------------------------- constants
const INPUT_CAP_UNITS: usize = 4_000_000;
const POOL_CAP_UNITS: usize = 4_000_000;
const OUT_CAP_BYTES: usize = 16_000_000; // 6 cases ~11.6MB JSON in the direct driver
const MAX_FIELDS: usize = 512_000;
const MAX_ROWS: usize = 64_000;
const MAX_ERRORS: usize = 32_000;

const INPUT_BYTES: usize = INPUT_CAP_UNITS * 2;
const POOL_BYTES: usize = POOL_CAP_UNITS * 2;

const COMMA: u16 = 0x2C;
const QUOTE: u16 = 0x22;
const LF: u16 = 0x0A;
const CR: u16 = 0x0D;
const BACKSLASH: u16 = 0x5C;
const BOM: u16 = 0xFEFF;

const FE_COMMA: u8 = 0;
const FE_LB: u8 = 1;
const FE_END: u8 = 2;

const K_COMMA: u8 = 0;
const K_QUOTE: u8 = 1;
const K_LF: u8 = 2;
const K_CR: u8 = 3;
const K_END: u8 = 4;

const E_STRAY_CR: u8 = 0;
const E_QUOTE_JUNK: u8 = 1;
const E_UNTERMINATED: u8 = 2;
const E_SHORT_ROW: u8 = 3;
const E_LONG_ROW: u8 = 4;
const CODE_NAMES: [&str; 5] = [
    "stray_cr",
    "quote_junk",
    "unterminated_quote",
    "short_row",
    "long_row",
];

// ---------------------------------------------------------------- arena
const INPUT_OFF: usize = 0;
const POOL_OFF: usize = INPUT_OFF + INPUT_BYTES;
const OUT_OFF: usize = POOL_OFF + POOL_BYTES;
const FIELDS_OFF: usize = OUT_OFF + OUT_CAP_BYTES;
const ROWS_OFF: usize = FIELDS_OFF + MAX_FIELDS * 8;
const ERRORS_OFF: usize = ROWS_OFF + MAX_ROWS * 8;
const TOTAL_BYTES: usize = ERRORS_OFF + MAX_ERRORS * 20;

static mut MEM: [u8; TOTAL_BYTES] = [0; TOTAL_BYTES];

#[inline(always)]
fn in_ptr() -> *const u16 {
    unsafe { MEM.as_ptr().add(INPUT_OFF) as *const u16 }
}
#[inline(always)]
fn pool_ptr() -> *mut u16 {
    unsafe { MEM.as_mut_ptr().add(POOL_OFF) as *mut u16 }
}
#[inline(always)]
fn out_ptr() -> *mut u8 {
    unsafe { MEM.as_mut_ptr().add(OUT_OFF) }
}
#[inline(always)]
fn fields_ptr() -> *mut (u32, u32) {
    unsafe { MEM.as_mut_ptr().add(FIELDS_OFF) as *mut (u32, u32) }
}
#[inline(always)]
fn rows_ptr() -> *mut (u32, u32) {
    unsafe { MEM.as_mut_ptr().add(ROWS_OFF) as *mut (u32, u32) }
}
#[inline(always)]
fn errors_ptr() -> *mut Error {
    unsafe { MEM.as_mut_ptr().add(ERRORS_OFF) as *mut Error }
}
#[inline(always)]
fn errors_ptr_const() -> *const Error {
    unsafe { MEM.as_ptr().add(ERRORS_OFF) as *const Error }
}

#[derive(Clone, Copy)]
struct Error {
    code: u8,
    line: u32,
    row: i32,      // -1 = null (header record)
    payload: u32,  // short_row: field count; long_row: extra field start
    payload2: u32, // long_row: extra field count
}

// ---------------------------------------------------------------- parser state
struct P {
    len: usize,
    pos: usize,
    line: u32,
    record_line: u32,
    record_row: i32,
    width: i32,
    header: bool,
    columns_done: bool,
    columns_start: u32,
    columns_count: u32,
    pool_pos: usize,
    nfields: usize,
    nrows: usize,
    nerrors: usize,
    field_start: usize,
    out_len: usize,
    out_base: usize,
    // Hoisted SIMD splats (computed once per parse).
    sp_comma: v128,
    sp_quote: v128,
    sp_lf: v128,
    sp_cr: v128,
    sp_bs: v128,
    sp_ctrl: v128,
    sp_hi: v128,
    sp_zero: v128,
}

impl P {
    #[inline(always)]
    fn inp(&self, i: usize) -> u16 {
        unsafe { *in_ptr().add(i) }
    }
    #[inline(always)]
    fn pool_push(&mut self, u: u16) {
        unsafe {
            *pool_ptr().add(self.pool_pos) = u;
        }
        self.pool_pos += 1;
    }
    #[inline(always)]
    fn pool_copy(&mut self, start: usize, end: usize) {
        let n = end - start;
        if n == 0 {
            return;
        }
        unsafe {
            core::ptr::copy_nonoverlapping(
                in_ptr().add(start),
                pool_ptr().add(self.pool_pos),
                n,
            );
        }
        self.pool_pos += n;
    }
    #[inline(always)]
    fn push_field(&mut self) {
        let off = self.field_start as u32;
        let n = (self.pool_pos - self.field_start) as u32;
        unsafe {
            *fields_ptr().add(self.nfields) = (off, n);
        }
        self.nfields += 1;
    }
    #[inline(always)]
    fn push_error(&mut self, code: u8, payload: u32, payload2: u32) {
        if self.nerrors >= MAX_ERRORS {
            return;
        }
        unsafe {
            *errors_ptr().add(self.nerrors) = Error {
                code,
                line: self.record_line,
                row: self.record_row,
                payload,
                payload2,
            };
        }
        self.nerrors += 1;
    }

    // --- specialised SIMD scans (each computes only the masks it needs) ---

    // First of {comma, LF, CR} at or after `from`. (read_unquoted: a quote
    // is a literal there, so it is not a delimiter.)
    #[inline(always)]
    fn scan3(&self, from: usize) -> (usize, u8) {
        let base = in_ptr();
        let mut i = from;
        while i + 8 <= self.len {
            let v = unsafe { v128_load(base.add(i) as *const v128) };
            let m = v128_or(
                v128_or(u16x8_eq(v, self.sp_comma), u16x8_eq(v, self.sp_lf)),
                u16x8_eq(v, self.sp_cr),
            );
            let bits = u16x8_bitmask(m);
            if bits != 0 {
                let lane = bits.trailing_zeros() as usize;
                let bit = 1u8 << lane;
                let p = i + lane;
                if (u16x8_bitmask(u16x8_eq(v, self.sp_comma)) & bit) != 0 {
                    return (p, K_COMMA);
                }
                if (u16x8_bitmask(u16x8_eq(v, self.sp_lf)) & bit) != 0 {
                    return (p, K_LF);
                }
                return (p, K_CR);
            }
            i += 8;
        }
        while i < self.len {
            let u = self.inp(i);
            if u == COMMA {
                return (i, K_COMMA);
            }
            if u == LF {
                return (i, K_LF);
            }
            if u == CR {
                return (i, K_CR);
            }
            i += 1;
        }
        (self.len, K_END)
    }

    // First quote at or after `from`, counting LFs and noting any CR in the
    // units scanned before it. Returns (pos, kind, lf_count, has_cr).
    // Used by read_quoted so the span is scanned exactly once.
    #[inline(always)]
    fn scan_quote(&self, from: usize) -> (usize, u8, u32, bool) {
        let base = in_ptr();
        let mut i = from;
        let mut lf: u32 = 0;
        let mut has_cr = false;
        while i + 8 <= self.len {
            let v = unsafe { v128_load(base.add(i) as *const v128) };
            let qm = u16x8_eq(v, self.sp_quote);
            let qbits = u16x8_bitmask(qm);
            if qbits != 0 {
                let lane = qbits.trailing_zeros() as usize;
                let before = (1u16 << lane) - 1;
                lf += (u16x8_bitmask(u16x8_eq(v, self.sp_lf)) & (before as u8))
                    .count_ones();
                has_cr = has_cr
                    || (u16x8_bitmask(u16x8_eq(v, self.sp_cr)) & (before as u8)) != 0;
                return (i + lane, K_QUOTE, lf, has_cr);
            }
            lf += u16x8_bitmask(u16x8_eq(v, self.sp_lf)).count_ones();
            has_cr = has_cr || u16x8_bitmask(u16x8_eq(v, self.sp_cr)) != 0;
            i += 8;
        }
        while i < self.len {
            let u = self.inp(i);
            if u == QUOTE {
                return (i, K_QUOTE, lf, has_cr);
            }
            if u == LF {
                lf += 1;
            } else if u == CR {
                has_cr = true;
            }
            i += 1;
        }
        (self.len, K_END, lf, has_cr)
    }

    // First LF at or after `from`, or (len, K_END).
    #[inline(always)]
    fn scan_lf(&self, from: usize) -> (usize, u8) {
        let base = in_ptr();
        let mut i = from;
        while i + 8 <= self.len {
            let v = unsafe { v128_load(base.add(i) as *const v128) };
            let bits = u16x8_bitmask(u16x8_eq(v, self.sp_lf));
            if bits != 0 {
                return (i + bits.trailing_zeros() as usize, K_LF);
            }
            i += 8;
        }
        while i < self.len {
            if self.inp(i) == LF {
                return (i, K_LF);
            }
            i += 1;
        }
        (self.len, K_END)
    }

    // First of {quote, CR} in [from, end). Returns (pos, kind).
    #[inline(always)]
    fn scan_quote_cr(&self, from: usize, end: usize) -> (usize, u8) {
        let base = in_ptr();
        let mut i = from;
        while i + 8 <= end {
            let v = unsafe { v128_load(base.add(i) as *const v128) };
            let m = v128_or(u16x8_eq(v, self.sp_quote), u16x8_eq(v, self.sp_cr));
            let bits = u16x8_bitmask(m);
            if bits != 0 {
                let lane = bits.trailing_zeros() as usize;
                let bit = 1u8 << lane;
                let p = i + lane;
                if (u16x8_bitmask(u16x8_eq(v, self.sp_quote)) & bit) != 0 {
                    return (p, K_QUOTE);
                }
                return (p, K_CR);
            }
            i += 8;
        }
        while i < end {
            let u = self.inp(i);
            if u == QUOTE {
                return (i, K_QUOTE);
            }
            if u == CR {
                return (i, K_CR);
            }
            i += 1;
        }
        (end, K_END)
    }

    // First comma in [from, end). Returns (pos, kind).
    #[inline(always)]
    fn scan_comma(&self, from: usize, end: usize) -> (usize, u8) {
        let base = in_ptr();
        let mut i = from;
        while i + 8 <= end {
            let v = unsafe { v128_load(base.add(i) as *const v128) };
            let bits = u16x8_bitmask(u16x8_eq(v, self.sp_comma));
            if bits != 0 {
                return (i + bits.trailing_zeros() as usize, K_COMMA);
            }
            i += 8;
        }
        while i < end {
            if self.inp(i) == COMMA {
                return (i, K_COMMA);
            }
            i += 1;
        }
        (end, K_END)
    }

    // Count lone CRs (CR not followed by LF) in input[start..end).
    // Single scalar pass; only called when a CR is known to be present.
    fn count_lone_cr(&self, start: usize, end: usize) -> u32 {
        let mut n: u32 = 0;
        let mut i = start;
        while i < end {
            if self.inp(i) == CR {
                if i + 1 >= end || self.inp(i + 1) != LF {
                    n += 1;
                }
            }
            i += 1;
        }
        n
    }

    // Reads unquoted characters onto the current field (already started in
    // the pool at field_start) up to the next comma or line break.
    // A CR not followed by LF is dropped (stray_cr). A quote is literal.
    #[inline(always)]
    fn read_unquoted(&mut self) -> u8 {
        loop {
            let (dpos, kind) = self.scan3(self.pos);
            match kind {
                K_COMMA => {
                    self.pool_copy(self.pos, dpos);
                    self.pos = dpos + 1;
                    return FE_COMMA;
                }
                K_LF => {
                    self.pool_copy(self.pos, dpos);
                    self.pos = dpos + 1;
                    self.line += 1;
                    return FE_LB;
                }
                K_CR => {
                    if dpos + 1 < self.len && self.inp(dpos + 1) == LF {
                        self.pool_copy(self.pos, dpos);
                        self.pos = dpos + 2;
                        self.line += 1;
                        return FE_LB;
                    }
                    self.push_error(E_STRAY_CR, 0, 0);
                    self.pool_copy(self.pos, dpos);
                    self.pos = dpos + 1;
                }
                _ => {
                    self.pool_copy(self.pos, self.len);
                    self.pos = self.len;
                    return FE_END;
                }
            }
        }
    }

    // Reads a quoted field whose opening quote is at pos.
    #[inline(always)]
    fn read_quoted(&mut self) -> u8 {
        let open = self.pos;
        let open_line = self.line;
        self.pos += 1;
        let mut has_doubled = false;
        let q: usize;
        // Scan for the closing quote, accumulating the span's LF count and
        // CR presence across legs (a doubled pair restarts the scan after
        // the pair; the pair's own units are quotes, so they add nothing).
        let mut scan = self.pos;
        let mut span_lf: u32 = 0;
        let mut span_has_cr = false;
        loop {
            let (p, k, lf, has_cr) = self.scan_quote(scan);
            span_lf += lf;
            span_has_cr = span_has_cr || has_cr;
            if k == K_END {
                // The quote never closes: literal character, text after it
                // read again as unquoted.
                self.pos = open + 1;
                self.line = open_line;
                self.push_error(E_UNTERMINATED, 0, 0);
                self.pool_push(QUOTE);
                return self.read_unquoted();
            }
            if p + 1 < self.len && self.inp(p + 1) == QUOTE {
                has_doubled = true;
                scan = p + 2;
                continue;
            }
            q = p;
            break;
        }
        let span_start = self.pos;
        let span_end = q;
        let breaks = span_lf
            + if span_has_cr {
                self.count_lone_cr(span_start, span_end)
            } else {
                0
            };
        if has_doubled || span_has_cr {
            // Collapse "" -> " and CRLF / lone CR -> LF, straight to pool.
            let mut i = span_start;
            while i < span_end {
                let u = self.inp(i);
                if u == QUOTE && i + 1 < span_end && self.inp(i + 1) == QUOTE {
                    self.pool_push(QUOTE);
                    i += 2;
                } else if u == CR {
                    self.pool_push(LF);
                    i += if i + 1 < span_end && self.inp(i + 1) == LF {
                        2
                    } else {
                        1
                    };
                } else {
                    self.pool_push(u);
                    i += 1;
                }
            }
        } else {
            self.pool_copy(span_start, span_end);
        }
        self.line += breaks;
        self.pos = q + 1;
        if self.pos >= self.len {
            return FE_END;
        }
        let c = self.inp(self.pos);
        if c == COMMA {
            self.pos += 1;
            return FE_COMMA;
        }
        if c == LF {
            self.pos += 1;
            self.line += 1;
            return FE_LB;
        }
        if c == CR && self.pos + 1 < self.len && self.inp(self.pos + 1) == LF {
            self.pos += 2;
            self.line += 1;
            return FE_LB;
        }
        // What follows the closing quote is appended as it stands.
        let before = (self.pool_pos - self.field_start) as u32;
        let fe = self.read_unquoted();
        if (self.pool_pos - self.field_start) as u32 > before {
            self.push_error(E_QUOTE_JUNK, 0, 0);
        }
        fe
    }

    fn finish_record(&mut self, fields_start: usize, n: usize) {
        if self.width < 0 {
            self.width = n as i32;
        }
        if self.header && !self.columns_done {
            self.columns_start = fields_start as u32;
            self.columns_count = n as u32;
            self.columns_done = true;
            return;
        }
        let w = self.width as usize;
        if n < w {
            self.push_error(E_SHORT_ROW, n as u32, 0);
            for _ in n..w {
                unsafe {
                    *fields_ptr().add(self.nfields) = (self.pool_pos as u32, 0);
                }
                self.nfields += 1;
            }
            self.push_row(fields_start, w);
        } else if n > w {
            let extra_start = fields_start + w;
            let extra_n = n - w;
            self.push_error(E_LONG_ROW, extra_start as u32, extra_n as u32);
            self.push_row(fields_start, w);
        } else {
            self.push_row(fields_start, n);
        }
    }

    #[inline(always)]
    fn push_row(&mut self, fields_start: usize, n: usize) {
        unsafe {
            *rows_ptr().add(self.nrows) = (fields_start as u32, n as u32);
        }
        self.nrows += 1;
    }

    fn run(&mut self) {
        if self.len > 0 && self.inp(0) == BOM {
            self.pos = 1;
        }
        loop {
            // Lines with no characters are no records.
            while self.pos < self.len {
                let u = self.inp(self.pos);
                if u == LF {
                    self.pos += 1;
                    self.line += 1;
                } else if u == CR
                    && self.pos + 1 < self.len
                    && self.inp(self.pos + 1) == LF
                {
                    self.pos += 2;
                    self.line += 1;
                } else {
                    break;
                }
            }
            if self.pos >= self.len {
                break;
            }
            self.record_line = self.line;
            self.record_row = if self.header && !self.columns_done {
                -1
            } else {
                self.nrows as i32
            };
            let rec_start = self.pos;

            // Fast path: the record's line holds no quote and no stray CR.
            let (line_end, has_lf) = match self.scan_lf(self.pos) {
                (p, K_LF) => (p, true),
                _ => (self.len, false),
            };
            let (dpos, dkind) = self.scan_quote_cr(self.pos, line_end);
            let mut fast = false;
            let mut content_end = line_end;
            if dkind == K_END {
                fast = true;
            } else if dkind == K_CR && has_lf && dpos == line_end - 1 {
                fast = true;
                content_end = line_end - 1;
            }
            if fast {
                let fstart = self.nfields;
                let mut s = rec_start;
                loop {
                    let (cpos, ck) = self.scan_comma(s, content_end);
                    self.field_start = self.pool_pos;
                    if ck == K_END {
                        self.pool_copy(s, content_end);
                        self.push_field();
                        break;
                    }
                    self.pool_copy(s, cpos);
                    self.push_field();
                    s = cpos + 1;
                }
                let n = self.nfields - fstart;
                self.pos = if has_lf { line_end + 1 } else { self.len };
                if has_lf {
                    self.line += 1;
                }
                self.finish_record(fstart, n);
                continue;
            }

            // Slow path: the record holds a quote or a stray CR.
            let fstart = self.nfields;
            let mut quoted = false;
            loop {
                quoted = self.inp(self.pos) == QUOTE;
                self.field_start = self.pool_pos;
                let f = if quoted {
                    self.read_quoted()
                } else {
                    self.read_unquoted()
                };
                self.push_field();
                if f != FE_COMMA {
                    break;
                }
            }
            let n = self.nfields - fstart;
            // A line that held only stray CRs is left with no characters:
            // no record (its stray_cr errors stay).
            if n == 1 && !quoted {
                let (off, flen) = unsafe { *fields_ptr().add(fstart) };
                let _ = off;
                if flen == 0 {
                    self.nfields -= 1;
                    continue;
                }
            }
            self.finish_record(fstart, n);
        }
    }

    // ---------------------------------------------------------------- JSON out
    #[inline(always)]
    fn out_byte(&mut self, b: u8) {
        unsafe {
            *out_ptr().add(self.out_len) = b;
        }
        self.out_len += 1;
    }
    #[inline(always)]
    fn out_bytes(&mut self, s: &[u8]) {
        let n = s.len();
        unsafe {
            core::ptr::copy_nonoverlapping(s.as_ptr(), out_ptr().add(self.out_len), n);
        }
        self.out_len += n;
    }
    fn out_u32(&mut self, mut v: u32) {
        let mut buf = [0u8; 10];
        let mut i = 10;
        if v == 0 {
            self.out_byte(b'0');
            return;
        }
        while v > 0 {
            i -= 1;
            buf[i] = b'0' + (v % 10) as u8;
            v /= 10;
        }
        self.out_bytes(&buf[i..]);
    }
    fn out_i32(&mut self, v: i32) {
        if v < 0 {
            self.out_byte(b'-');
            self.out_u32((-(v as i64)) as u32);
        } else {
            self.out_u32(v as u32);
        }
    }

    // Write one pool field as a JSON string (with quotes).
    // 16-unit SIMD bulk runs, an 8-unit bulk step, then a scalar tail.
    // Works on a local output pointer (no repeated address recomputation).
    #[inline(always)]
    fn write_json_string(&mut self, pool_off: usize, len: usize) {
        let base = unsafe { MEM.as_ptr().add(POOL_OFF) as *const u16 };
        let mut out = unsafe { out_ptr().add(self.out_len) };
        unsafe {
            *out = b'"';
            out = out.add(1);
        }
        let mut i = 0usize;
        while i + 16 <= len {
            let v0 = unsafe { v128_load(base.add(pool_off + i) as *const v128) };
            let v1 = unsafe { v128_load(base.add(pool_off + i + 8) as *const v128) };
            let bad0 = v128_or(
                v128_or(
                    u16x8_eq(v0, self.sp_quote),
                    u16x8_eq(v0, self.sp_bs),
                ),
                v128_or(
                    u16x8_lt(v0, self.sp_ctrl),
                    u16x8_ge(v0, self.sp_hi),
                ),
            );
            let bad1 = v128_or(
                v128_or(
                    u16x8_eq(v1, self.sp_quote),
                    u16x8_eq(v1, self.sp_bs),
                ),
                v128_or(
                    u16x8_lt(v1, self.sp_ctrl),
                    u16x8_ge(v1, self.sp_hi),
                ),
            );
            if u16x8_bitmask(bad0) | u16x8_bitmask(bad1) != 0 {
                break;
            }
            let packed = i8x16_narrow_i16x8(v0, v1);
            unsafe {
                v128_store(out as *mut v128, packed);
                out = out.add(16);
            }
            i += 16;
        }
        // 8-unit bulk step for the remaining middle (short fields live here).
        if i + 8 <= len {
            let v = unsafe { v128_load(base.add(pool_off + i) as *const v128) };
            let bad = v128_or(
                v128_or(
                    u16x8_eq(v, self.sp_quote),
                    u16x8_eq(v, self.sp_bs),
                ),
                v128_or(
                    u16x8_lt(v, self.sp_ctrl),
                    u16x8_ge(v, self.sp_hi),
                ),
            );
            if u16x8_bitmask(bad) == 0 {
                let packed = i8x16_narrow_i16x8(v, self.sp_zero);
                let low = i64x2_extract_lane::<0>(packed) as u64;
                unsafe {
                    *(out as *mut u64) = low;
                    out = out.add(8);
                }
                i += 8;
            }
        }
        // Scalar tail.
        let h = b"0123456789abcdef";
        while i < len {
            let u = unsafe { *base.add(pool_off + i) };
            if u < 0x20 {
                unsafe {
                    *out = b'\\';
                    *out.add(1) = b'u';
                    *out.add(2) = b'0';
                    *out.add(3) = b'0';
                    *out.add(4) = h[(u >> 4) as usize];
                    *out.add(5) = h[(u & 0xF) as usize];
                    out = out.add(6);
                }
                i += 1;
            } else if u == QUOTE {
                unsafe {
                    *out = b'\\';
                    *out.add(1) = b'"';
                    out = out.add(2);
                }
                i += 1;
            } else if u == BACKSLASH {
                unsafe {
                    *out = b'\\';
                    *out.add(1) = b'\\';
                    out = out.add(2);
                }
                i += 1;
            } else if u < 0x80 {
                unsafe {
                    *out = u as u8;
                    out = out.add(1);
                }
                i += 1;
            } else if (0xD800..0xDC00).contains(&u) {
                if i + 1 < len {
                    let u2 = unsafe { *base.add(pool_off + i + 1) };
                    if (0xDC00..0xE000).contains(&u2) {
                        let cp: u32 = 0x10000
                            + (((u as u32) - 0xD800) << 10)
                            + ((u2 as u32) - 0xDC00);
                        unsafe {
                            *out = (0xF0 | (cp >> 18)) as u8;
                            *out.add(1) = (0x80 | ((cp >> 12) & 0x3F)) as u8;
                            *out.add(2) = (0x80 | ((cp >> 6) & 0x3F)) as u8;
                            *out.add(3) = (0x80 | (cp & 0x3F)) as u8;
                            out = out.add(4);
                        }
                        i += 2;
                        continue;
                    }
                }
                unsafe {
                    *out = b'\\';
                    *out.add(1) = b'u';
                    *out.add(2) = h[((u >> 12) & 0xF) as usize];
                    *out.add(3) = h[((u >> 8) & 0xF) as usize];
                    *out.add(4) = h[((u >> 4) & 0xF) as usize];
                    *out.add(5) = h[(u & 0xF) as usize];
                    out = out.add(6);
                }
                i += 1;
            } else if (0xDC00..0xE000).contains(&u) {
                unsafe {
                    *out = b'\\';
                    *out.add(1) = b'u';
                    *out.add(2) = h[((u >> 12) & 0xF) as usize];
                    *out.add(3) = h[((u >> 8) & 0xF) as usize];
                    *out.add(4) = h[((u >> 4) & 0xF) as usize];
                    *out.add(5) = h[(u & 0xF) as usize];
                    out = out.add(6);
                }
                i += 1;
            } else if u < 0x800 {
                unsafe {
                    *out = (0xC0 | (u >> 6)) as u8;
                    *out.add(1) = (0x80 | (u & 0x3F)) as u8;
                    out = out.add(2);
                }
                i += 1;
            } else {
                unsafe {
                    *out = (0xE0 | (u >> 12)) as u8;
                    *out.add(1) = (0x80 | ((u >> 6) & 0x3F)) as u8;
                    *out.add(2) = (0x80 | (u & 0x3F)) as u8;
                    out = out.add(3);
                }
                i += 1;
            }
        }
        unsafe {
            *out = b'"';
            out = out.add(1);
        }
        self.out_len = unsafe { out.offset_from(out_ptr()) as usize };
    }

    #[inline(always)]
    fn write_fields_json(&mut self, start: usize, n: usize) {
        self.out_byte(b'[');
        for k in 0..n {
            if k > 0 {
                self.out_byte(b',');
            }
            let (off, flen) = unsafe { *fields_ptr().add(start + k) };
            self.write_json_string(off as usize, flen as usize);
        }
        self.out_byte(b']');
    }

    fn write_json(&mut self) {
        self.out_len = self.out_base;
        self.out_bytes(b"{\"columns\":");
        if self.header {
            if self.columns_done {
                self.write_fields_json(self.columns_start as usize, self.columns_count as usize);
            } else {
                self.out_bytes(b"[]");
            }
        } else {
            self.out_bytes(b"null");
        }
        self.out_bytes(b",\"rows\":[");
        for r in 0..self.nrows {
            if r > 0 {
                self.out_byte(b',');
            }
            let (fs, n) = unsafe { *rows_ptr().add(r) };
            self.write_fields_json(fs as usize, n as usize);
        }
        self.out_bytes(b"],\"errors\":[");
        for e in 0..self.nerrors {
            if e > 0 {
                self.out_byte(b',');
            }
            let err = unsafe { *errors_ptr_const().add(e) };
            self.out_bytes(b"{\"code\":\"");
            self.out_bytes(CODE_NAMES[err.code as usize].as_bytes());
            self.out_bytes(b"\",\"line\":");
            self.out_u32(err.line);
            self.out_bytes(b",\"row\":");
            if err.row < 0 {
                self.out_bytes(b"null");
            } else {
                self.out_u32(err.row as u32);
            }
            match err.code {
                E_SHORT_ROW => {
                    self.out_bytes(b",\"fields\":");
                    self.out_u32(err.payload);
                }
                E_LONG_ROW => {
                    self.out_bytes(b",\"extra\":");
                    self.write_fields_json(err.payload as usize, err.payload2 as usize);
                }
                _ => {}
            }
            self.out_byte(b'}');
        }
        self.out_bytes(b"]}");
    }
}

fn new_parser(len_units: usize, header: bool) -> P {
    P {
        len: len_units,
        pos: 0,
        line: 1,
        record_line: 1,
        record_row: -1,
        width: -1,
        header,
        columns_done: false,
        columns_start: 0,
        columns_count: 0,
        pool_pos: 0,
        nfields: 0,
        nrows: 0,
        nerrors: 0,
        field_start: 0,
        out_len: 0,
        out_base: 0,
        sp_comma: u16x8_splat(COMMA),
        sp_quote: u16x8_splat(QUOTE),
        sp_lf: u16x8_splat(LF),
        sp_cr: u16x8_splat(CR),
        sp_bs: u16x8_splat(BACKSLASH),
        sp_ctrl: u16x8_splat(0x20),
        sp_hi: u16x8_splat(0x80),
        sp_zero: u16x8_splat(0),
    }
}

// ---------------------------------------------------------------- exports
#[no_mangle]
pub extern "C" fn input_ptr() -> u32 {
    in_ptr() as u32
}

#[no_mangle]
pub extern "C" fn output_ptr() -> u32 {
    out_ptr() as u32
}

#[no_mangle]
pub extern "C" fn parse(len_units: u32, header: u32) -> u32 {
    parse_at(len_units, header, 0)
}

/// Parse and write the JSON answer at OUT + out_off (bytes); returns its length.
/// Lets a driver lay several cases' JSON out contiguously and write once.
#[no_mangle]
pub extern "C" fn parse_at(len_units: u32, header: u32, out_off: u32) -> u32 {
    let mut p = new_parser(len_units as usize, header != 0);
    p.out_base = out_off as usize;
    p.run();
    p.write_json();
    (p.out_len - p.out_base) as u32
}

// Temporary profiling export: parse but skip the JSON write.
#[no_mangle]
pub extern "C" fn parse_nojson(len_units: u32, header: u32) -> u32 {
    let mut p = new_parser(len_units as usize, header != 0);
    p.run();
    p.nfields as u32
}

// ---------------------------------------------------------------- fused path
// Single-pass fused transducer: scan + field extraction + JSON write in ONE
// pass over the input. No POOL / FIELDS / ROWS arrays: field JSON is written
// straight from the input spans into OUT, errors stay in the small binary
// ERRORS buffer, long-row extra fields go to the EXTRA scratch (JSON text).
// If EXTRA runs low, parse_fused_at returns FUSED_OVERFLOW and the driver
// falls back to the two-phase parse_at (whose regions are kept intact).
// The old parse/parse_at exports stay untouched (verified path + fallback).

const EXTRA_CAP_BYTES: usize = 1_048_576;
const EXTRA_RESERVE_BYTES: usize = 16_384; // refuse a new extra field below this
const FUSED_OVERFLOW: u32 = u32::MAX;

static mut EXTRA_MEM: [u8; EXTRA_CAP_BYTES] = [0; EXTRA_CAP_BYTES];

#[inline(always)]
fn wbyte(dst: *mut u8, b: u8) -> *mut u8 {
    unsafe {
        *dst = b;
        dst.add(1)
    }
}

#[inline(always)]
fn wbytes(dst: *mut u8, s: &[u8]) -> *mut u8 {
    unsafe {
        core::ptr::copy_nonoverlapping(s.as_ptr(), dst, s.len());
        dst.add(s.len())
    }
}

#[inline(always)]
fn wu32(dst: *mut u8, mut v: u32) -> *mut u8 {
    if v == 0 {
        return wbyte(dst, b'0');
    }
    let mut buf = [0u8; 10];
    let mut i = 10;
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    wbytes(dst, &buf[i..])
}

struct F {
    len: usize,
    pos: usize,
    line: u32,
    record_line: u32,
    record_row: i32,
    width: i32,
    header: bool,
    columns_done: bool,
    nrows: usize,
    nerrors: usize,
    out: *mut u8,
    out_base: *mut u8,
    xout: *mut u8,
    xbase: *mut u8,
    extra_overflow: bool,
    sp_comma: v128,
    sp_quote: v128,
    sp_lf: v128,
    sp_cr: v128,
    sp_bs: v128,
    sp_ctrl: v128,
    sp_hi: v128,
    sp_zero: v128,
}

impl F {
    #[inline(always)]
    fn inp(&self, i: usize) -> u16 {
        unsafe { *in_ptr().add(i) }
    }
    #[inline(always)]
    fn push_error(&mut self, code: u8, payload: u32, payload2: u32) {
        if self.nerrors >= MAX_ERRORS {
            return;
        }
        unsafe {
            *errors_ptr().add(self.nerrors) = Error {
                code,
                line: self.record_line,
                row: self.record_row,
                payload,
                payload2,
            };
        }
        self.nerrors += 1;
    }

    // First of {comma, LF, CR} at or after `from`. (Same as P::scan3.)
    #[inline(always)]
    fn scan3(&self, from: usize) -> (usize, u8) {
        let base = in_ptr();
        let mut i = from;
        while i + 8 <= self.len {
            let v = unsafe { v128_load(base.add(i) as *const v128) };
            let m = v128_or(
                v128_or(u16x8_eq(v, self.sp_comma), u16x8_eq(v, self.sp_lf)),
                u16x8_eq(v, self.sp_cr),
            );
            let bits = u16x8_bitmask(m);
            if bits != 0 {
                let lane = bits.trailing_zeros() as usize;
                let bit = 1u8 << lane;
                let p = i + lane;
                if (u16x8_bitmask(u16x8_eq(v, self.sp_comma)) & bit) != 0 {
                    return (p, K_COMMA);
                }
                if (u16x8_bitmask(u16x8_eq(v, self.sp_lf)) & bit) != 0 {
                    return (p, K_LF);
                }
                return (p, K_CR);
            }
            i += 8;
        }
        while i < self.len {
            let u = self.inp(i);
            if u == COMMA {
                return (i, K_COMMA);
            }
            if u == LF {
                return (i, K_LF);
            }
            if u == CR {
                return (i, K_CR);
            }
            i += 1;
        }
        (self.len, K_END)
    }

    // First quote at or after `from`, counting LFs and noting any CR in the
    // units scanned before it. (Same as P::scan_quote.)
    #[inline(always)]
    fn scan_quote(&self, from: usize) -> (usize, u8, u32, bool) {
        let base = in_ptr();
        let mut i = from;
        let mut lf: u32 = 0;
        let mut has_cr = false;
        while i + 8 <= self.len {
            let v = unsafe { v128_load(base.add(i) as *const v128) };
            let qm = u16x8_eq(v, self.sp_quote);
            let qbits = u16x8_bitmask(qm);
            if qbits != 0 {
                let lane = qbits.trailing_zeros() as usize;
                let before = (1u16 << lane) - 1;
                lf += (u16x8_bitmask(u16x8_eq(v, self.sp_lf)) & (before as u8))
                    .count_ones();
                has_cr = has_cr
                    || (u16x8_bitmask(u16x8_eq(v, self.sp_cr)) & (before as u8)) != 0;
                return (i + lane, K_QUOTE, lf, has_cr);
            }
            lf += u16x8_bitmask(u16x8_eq(v, self.sp_lf)).count_ones();
            has_cr = has_cr || u16x8_bitmask(u16x8_eq(v, self.sp_cr)) != 0;
            i += 8;
        }
        while i < self.len {
            let u = self.inp(i);
            if u == QUOTE {
                return (i, K_QUOTE, lf, has_cr);
            }
            if u == LF {
                lf += 1;
            } else if u == CR {
                has_cr = true;
            }
            i += 1;
        }
        (self.len, K_END, lf, has_cr)
    }

    // Escape one UTF-16 unit as JSON string content (no surrounding quotes).
    // `next`/`has_next` are the following unit for surrogate pairs.
    // Returns (dst, units_consumed).
    #[inline(always)]
    fn write_unit_json(
        &self,
        mut dst: *mut u8,
        u: u16,
        next: u16,
        has_next: bool,
    ) -> (*mut u8, usize) {
        let h = b"0123456789abcdef";
        if u < 0x20 {
            dst = wbytes(dst, b"\\u00");
            dst = wbyte(dst, h[(u >> 4) as usize]);
            dst = wbyte(dst, h[(u & 0xF) as usize]);
            (dst, 1)
        } else if u == QUOTE {
            dst = wbytes(dst, b"\\\"");
            (dst, 1)
        } else if u == BACKSLASH {
            dst = wbytes(dst, b"\\\\");
            (dst, 1)
        } else if u < 0x80 {
            dst = wbyte(dst, u as u8);
            (dst, 1)
        } else if (0xD800..0xDC00).contains(&u) {
            if has_next && (0xDC00..0xE000).contains(&next) {
                let cp: u32 =
                    0x10000 + (((u as u32) - 0xD800) << 10) + ((next as u32) - 0xDC00);
                dst = wbyte(dst, (0xF0 | (cp >> 18)) as u8);
                dst = wbyte(dst, (0x80 | ((cp >> 12) & 0x3F)) as u8);
                dst = wbyte(dst, (0x80 | ((cp >> 6) & 0x3F)) as u8);
                dst = wbyte(dst, (0x80 | (cp & 0x3F)) as u8);
                (dst, 2)
            } else {
                dst = wbytes(dst, b"\\u");
                dst = wbyte(dst, h[((u >> 12) & 0xF) as usize]);
                dst = wbyte(dst, h[((u >> 8) & 0xF) as usize]);
                dst = wbyte(dst, h[((u >> 4) & 0xF) as usize]);
                dst = wbyte(dst, h[(u & 0xF) as usize]);
                (dst, 1)
            }
        } else if (0xDC00..0xE000).contains(&u) {
            dst = wbytes(dst, b"\\u");
            dst = wbyte(dst, h[((u >> 12) & 0xF) as usize]);
            dst = wbyte(dst, h[((u >> 8) & 0xF) as usize]);
            dst = wbyte(dst, h[((u >> 4) & 0xF) as usize]);
            dst = wbyte(dst, h[(u & 0xF) as usize]);
            (dst, 1)
        } else if u < 0x800 {
            dst = wbyte(dst, (0xC0 | (u >> 6)) as u8);
            dst = wbyte(dst, (0x80 | (u & 0x3F)) as u8);
            (dst, 1)
        } else {
            dst = wbyte(dst, (0xE0 | (u >> 12)) as u8);
            dst = wbyte(dst, (0x80 | ((u >> 6) & 0x3F)) as u8);
            dst = wbyte(dst, (0x80 | (u & 0x3F)) as u8);
            (dst, 1)
        }
    }

    // Write input[start..end] as JSON string content (no surrounding quotes).
    // SIMD bulk for clean ASCII runs, scalar tail for the rest. (Same
    // escaping as P::write_json_string, but over input spans, into `dst`.)
    #[inline(always)]
    fn write_span_json(&self, mut dst: *mut u8, start: usize, end: usize) -> *mut u8 {
        let base = in_ptr();
        let len = end - start;
        let mut i = 0usize;
        while i + 16 <= len {
            let v0 = unsafe { v128_load(base.add(start + i) as *const v128) };
            let v1 = unsafe { v128_load(base.add(start + i + 8) as *const v128) };
            let bad0 = v128_or(
                v128_or(u16x8_eq(v0, self.sp_quote), u16x8_eq(v0, self.sp_bs)),
                v128_or(u16x8_lt(v0, self.sp_ctrl), u16x8_ge(v0, self.sp_hi)),
            );
            let bad1 = v128_or(
                v128_or(u16x8_eq(v1, self.sp_quote), u16x8_eq(v1, self.sp_bs)),
                v128_or(u16x8_lt(v1, self.sp_ctrl), u16x8_ge(v1, self.sp_hi)),
            );
            if u16x8_bitmask(bad0) | u16x8_bitmask(bad1) != 0 {
                break;
            }
            let packed = i8x16_narrow_i16x8(v0, v1);
            unsafe {
                v128_store(dst as *mut v128, packed);
                dst = dst.add(16);
            }
            i += 16;
        }
        if i + 8 <= len {
            let v = unsafe { v128_load(base.add(start + i) as *const v128) };
            let bad = v128_or(
                v128_or(u16x8_eq(v, self.sp_quote), u16x8_eq(v, self.sp_bs)),
                v128_or(u16x8_lt(v, self.sp_ctrl), u16x8_ge(v, self.sp_hi)),
            );
            if u16x8_bitmask(bad) == 0 {
                let packed = i8x16_narrow_i16x8(v, self.sp_zero);
                let low = i64x2_extract_lane::<0>(packed) as u64;
                unsafe {
                    *(dst as *mut u64) = low;
                    dst = dst.add(8);
                }
                i += 8;
            }
        }
        while i < len {
            let u = unsafe { *base.add(start + i) };
            let next = if i + 1 < len {
                unsafe { *base.add(start + i + 1) }
            } else {
                0
            };
            let (d2, adv) = self.write_unit_json(dst, u, next, i + 1 < len);
            dst = d2;
            i += adv;
        }
        dst
    }

    // Write a quoted span's raw content with "" -> " and CRLF / lone CR ->
    // LF collapsing, JSON-escaped. Returns (dst, line breaks in the span).
    fn write_span_collapse(
        &self,
        mut dst: *mut u8,
        start: usize,
        end: usize,
    ) -> (*mut u8, u32) {
        let base = in_ptr();
        let mut breaks: u32 = 0;
        let mut i = start;
        while i < end {
            let u = unsafe { *base.add(i) };
            if u == QUOTE {
                // Inside the span every quote is a doubled pair (the scan
                // only stops at a quote not followed by another quote).
                dst = wbytes(dst, b"\\\"");
                i += if i + 1 < end && unsafe { *base.add(i + 1) } == QUOTE {
                    2
                } else {
                    1
                };
            } else if u == CR {
                dst = wbytes(dst, b"\\u000a");
                breaks += 1;
                i += if i + 1 < end && unsafe { *base.add(i + 1) } == LF {
                    2
                } else {
                    1
                };
            } else if u == LF {
                dst = wbytes(dst, b"\\u000a");
                breaks += 1;
                i += 1;
            } else {
                let next = if i + 1 < end {
                    unsafe { *base.add(i + 1) }
                } else {
                    0
                };
                let (d2, adv) = self.write_unit_json(dst, u, next, i + 1 < end);
                dst = d2;
                i += adv;
            }
        }
        (dst, breaks)
    }

    // Reads unquoted characters as the current field's content (already
    // inside the JSON string quotes), up to the next comma or line break.
    // A CR not followed by LF is dropped (stray_cr). A quote is literal.
    #[inline(always)]
    fn read_unquoted(&mut self, mut dst: *mut u8) -> (*mut u8, u8) {
        loop {
            let (dpos, kind) = self.scan3(self.pos);
            match kind {
                K_COMMA => {
                    dst = self.write_span_json(dst, self.pos, dpos);
                    self.pos = dpos + 1;
                    return (dst, FE_COMMA);
                }
                K_LF => {
                    dst = self.write_span_json(dst, self.pos, dpos);
                    self.pos = dpos + 1;
                    self.line += 1;
                    return (dst, FE_LB);
                }
                K_CR => {
                    if dpos + 1 < self.len && self.inp(dpos + 1) == LF {
                        dst = self.write_span_json(dst, self.pos, dpos);
                        self.pos = dpos + 2;
                        self.line += 1;
                        return (dst, FE_LB);
                    }
                    self.push_error(E_STRAY_CR, 0, 0);
                    dst = self.write_span_json(dst, self.pos, dpos);
                    self.pos = dpos + 1;
                }
                _ => {
                    dst = self.write_span_json(dst, self.pos, self.len);
                    self.pos = self.len;
                    return (dst, FE_END);
                }
            }
        }
    }

    // Reads a quoted field whose opening quote is at pos, as the current
    // field's content (already inside the JSON string quotes).
    #[inline(always)]
    fn read_quoted(&mut self, mut dst: *mut u8) -> (*mut u8, u8) {
        let open = self.pos;
        let open_line = self.line;
        self.pos += 1;
        let mut has_doubled = false;
        let q: usize;
        let mut scan = self.pos;
        let mut span_lf: u32 = 0;
        let mut span_has_cr = false;
        loop {
            let (p, k, lf, has_cr) = self.scan_quote(scan);
            span_lf += lf;
            span_has_cr = span_has_cr || has_cr;
            if k == K_END {
                // The quote never closes: literal character, text after it
                // read again as unquoted.
                self.pos = open + 1;
                self.line = open_line;
                self.push_error(E_UNTERMINATED, 0, 0);
                dst = wbytes(dst, b"\\\"");
                return self.read_unquoted(dst);
            }
            if p + 1 < self.len && self.inp(p + 1) == QUOTE {
                has_doubled = true;
                scan = p + 2;
                continue;
            }
            q = p;
            break;
        }
        let span_start = self.pos;
        let span_end = q;
        if has_doubled || span_has_cr {
            let (d2, breaks) = self.write_span_collapse(dst, span_start, span_end);
            dst = d2;
            self.line += breaks;
        } else {
            dst = self.write_span_json(dst, span_start, span_end);
            self.line += span_lf;
        }
        self.pos = q + 1;
        if self.pos >= self.len {
            return (dst, FE_END);
        }
        let c = self.inp(self.pos);
        if c == COMMA {
            self.pos += 1;
            return (dst, FE_COMMA);
        }
        if c == LF {
            self.pos += 1;
            self.line += 1;
            return (dst, FE_LB);
        }
        if c == CR && self.pos + 1 < self.len && self.inp(self.pos + 1) == LF {
            self.pos += 2;
            self.line += 1;
            return (dst, FE_LB);
        }
        // What follows the closing quote is appended as it stands.
        let before = dst;
        let (d2, fe) = self.read_unquoted(dst);
        dst = d2;
        if dst != before {
            self.push_error(E_QUOTE_JUNK, 0, 0);
        }
        (dst, fe)
    }

    // Parses one record and writes its JSON array. `is_columns` means this
    // record is the header's columns (header && !columns_done). Returns
    // true iff the record is real (a line left with no characters is no
    // record; its stray_cr errors stay, OUT/EXTRA are rewound).
    fn parse_record(&mut self, is_columns: bool) -> bool {
        self.record_line = self.line;
        self.record_row = if is_columns {
            -1
        } else {
            self.nrows as i32
        };
        let mark = self.out;
        let xmark = self.xout;
        if is_columns {
            self.out = wbyte(self.out, b'[');
        } else {
            if self.nrows > 0 {
                self.out = wbyte(self.out, b',');
            }
            self.out = wbyte(self.out, b'[');
        }
        let mut k = 0usize;
        let mut first_quoted = false;
        let mut first_len = 0usize;
        let mut extra_count = 0usize;
        let mut extra_start = self.xout;
        let mut fe: u8 = FE_END;
        loop {
            let quoted = self.inp(self.pos) == QUOTE;
            if k == 0 {
                first_quoted = quoted;
            }
            let is_extra = !is_columns && self.width >= 0 && (k as i32) >= self.width;
            if is_extra
                && (self.xout as usize) - (self.xbase as usize)
                    > EXTRA_CAP_BYTES - EXTRA_RESERVE_BYTES
            {
                self.extra_overflow = true;
                return false;
            }
            let mut dst: *mut u8;
            if is_extra {
                if extra_count == 0 {
                    extra_start = self.xout;
                }
                dst = self.xout;
                if extra_count > 0 {
                    dst = wbyte(dst, b',');
                }
            } else {
                dst = self.out;
                if k > 0 {
                    dst = wbyte(dst, b',');
                }
            }
            let content_start = dst;
            dst = wbyte(dst, b'"');
            let (d2, f) = if quoted {
                self.read_quoted(dst)
            } else {
                self.read_unquoted(dst)
            };
            dst = d2;
            dst = wbyte(dst, b'"');
            if k == 0 {
                first_len = (dst as usize) - (content_start as usize) - 2;
            }
            if is_extra {
                self.xout = dst;
                extra_count += 1;
            } else {
                self.out = dst;
            }
            fe = f;
            k += 1;
            if fe != FE_COMMA {
                break;
            }
        }
        if k == 1 && !first_quoted && first_len == 0 {
            self.out = mark;
            self.xout = xmark;
            return false;
        }
        if is_columns {
            if self.width < 0 {
                self.width = k as i32;
            }
            self.out = wbyte(self.out, b']');
            self.columns_done = true;
            return true;
        }
        if self.width < 0 {
            self.width = k as i32;
            self.out = wbyte(self.out, b']');
            self.nrows += 1;
            return true;
        }
        let w = self.width as usize;
        if k < w {
            self.push_error(E_SHORT_ROW, k as u32, 0);
            for _ in k..w {
                self.out = wbytes(self.out, b",\"\"");
            }
            self.out = wbyte(self.out, b']');
        } else if k > w {
            let xlen = (self.xout as usize) - (extra_start as usize);
            self.push_error(
                E_LONG_ROW,
                (extra_start as usize - self.xbase as usize) as u32,
                xlen as u32,
            );
            self.out = wbyte(self.out, b']');
        } else {
            self.out = wbyte(self.out, b']');
        }
        self.nrows += 1;
        true
    }

    // Skips lines left with no characters (LF / CRLF only).
    #[inline(always)]
    fn skip_empty_lines(&mut self) {
        while self.pos < self.len {
            let u = self.inp(self.pos);
            if u == LF {
                self.pos += 1;
                self.line += 1;
            } else if u == CR && self.pos + 1 < self.len && self.inp(self.pos + 1) == LF {
                self.pos += 2;
                self.line += 1;
            } else {
                break;
            }
        }
    }

    fn run_fused(&mut self) {
        if self.len > 0 && self.inp(0) == BOM {
            self.pos = 1;
        }
        if self.header {
            self.out = wbytes(self.out, b"{\"columns\":");
            loop {
                self.skip_empty_lines();
                if self.pos >= self.len {
                    break;
                }
                if self.parse_record(true) {
                    break;
                }
                if self.extra_overflow {
                    return;
                }
            }
            if !self.columns_done {
                self.out = wbytes(self.out, b"[]");
            }
            self.out = wbytes(self.out, b",\"rows\":[");
        } else {
            self.out = wbytes(self.out, b"{\"columns\":null,\"rows\":[");
        }
        loop {
            self.skip_empty_lines();
            if self.pos >= self.len {
                break;
            }
            self.parse_record(false);
            if self.extra_overflow {
                return;
            }
        }
        self.out = wbytes(self.out, b"],\"errors\":[");
        for e in 0..self.nerrors {
            if e > 0 {
                self.out = wbyte(self.out, b',');
            }
            let err = unsafe { *errors_ptr_const().add(e) };
            self.out = wbytes(self.out, b"{\"code\":\"");
            self.out = wbytes(self.out, CODE_NAMES[err.code as usize].as_bytes());
            self.out = wbytes(self.out, b"\",\"line\":");
            self.out = wu32(self.out, err.line);
            self.out = wbytes(self.out, b",\"row\":");
            if err.row < 0 {
                self.out = wbytes(self.out, b"null");
            } else {
                self.out = wu32(self.out, err.row as u32);
            }
            match err.code {
                E_SHORT_ROW => {
                    self.out = wbytes(self.out, b",\"fields\":");
                    self.out = wu32(self.out, err.payload);
                }
                E_LONG_ROW => {
                    self.out = wbytes(self.out, b",\"extra\":[");
                    let xs = unsafe { EXTRA_MEM.as_ptr().add(err.payload as usize) };
                    let xn = err.payload2 as usize;
                    unsafe {
                        core::ptr::copy_nonoverlapping(xs, self.out, xn);
                        self.out = self.out.add(xn);
                    }
                    self.out = wbyte(self.out, b']');
                }
                _ => {}
            }
            self.out = wbyte(self.out, b'}');
        }
        self.out = wbytes(self.out, b"]}");
    }
}

fn new_fused(len_units: usize, header: bool, out_off: usize) -> F {
    let out_base = unsafe { MEM.as_mut_ptr().add(OUT_OFF + out_off) };
    let xbase = unsafe { EXTRA_MEM.as_mut_ptr() };
    F {
        len: len_units,
        pos: 0,
        line: 1,
        record_line: 1,
        record_row: -1,
        width: -1,
        header,
        columns_done: false,
        nrows: 0,
        nerrors: 0,
        out: out_base,
        out_base,
        xout: xbase,
        xbase,
        extra_overflow: false,
        sp_comma: u16x8_splat(COMMA),
        sp_quote: u16x8_splat(QUOTE),
        sp_lf: u16x8_splat(LF),
        sp_cr: u16x8_splat(CR),
        sp_bs: u16x8_splat(BACKSLASH),
        sp_ctrl: u16x8_splat(0x20),
        sp_hi: u16x8_splat(0x80),
        sp_zero: u16x8_splat(0),
    }
}

/// Fused single-pass parse: writes the JSON answer at OUT + out_off (bytes);
/// returns its length, or FUSED_OVERFLOW (u32::MAX) if the EXTRA scratch ran
/// low (driver falls back to parse_at). Bytes stay valid until the next
/// parse call.
#[no_mangle]
pub extern "C" fn parse_fused_at(len_units: u32, header: u32, out_off: u32) -> u32 {
    let mut f = new_fused(len_units as usize, header != 0, out_off as usize);
    f.run_fused();
    if f.extra_overflow {
        return FUSED_OVERFLOW;
    }
    (f.out as usize - f.out_base as usize) as u32
}
