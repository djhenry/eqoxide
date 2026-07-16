//! `WireReader` — the single cursor for decoding RoF2 wire packets.
//!
//! Before this existed, the packet decoders used four+ competing byte-reading idioms with three
//! DIFFERENT error behaviours (see the module history / `docs`):
//!   * `rd_u32`/`rd_u16`/`rd_u8`/`rd_cstr`/`rd_fixed_cstr` — silently returned `0`/`""` past the end
//!     (an *agent-honesty* hazard: a truncated packet decoded to plausible-looking garbage);
//!   * `read_cstr`/`take_u32` and the `need!`/`rd_*!` macros — returned `Option`/`None`;
//!   * scattered raw `u32::from_le_bytes([p[0],..])` — panicked on the slice index.
//!
//! `WireReader` unifies all of them. Its guiding rule (owner's explicit decision, the project's
//! agent-honesty invariant): **a required read that runs off the end PANICS**, loudly and with a
//! diagnostic naming the packet context, the offset, and bytes-needed-vs-remaining — because a loud
//! crash beats silently decoding garbage. For genuinely variable-length / optional-trailing fields
//! (count-driven record loops, flag-conditional trailers, "read if bytes remain") the cursor also
//! provides NON-panicking `try_*` / `has` / `remaining` paths so a *valid* short packet never
//! crashes. Callers pick the path that matches the field's contract.
//!
//! All integer reads are little-endian unless the method name ends in `_be` (big-endian — the one
//! guild packet that arrives in network byte order).

/// A forward-only cursor over a wire packet payload.
///
/// Construct with a `context` label (an opcode / struct name) — it is echoed in every panic message
/// so a crash is instantly attributable to the packet that caused it.
pub struct WireReader<'a> {
    buf: &'a [u8],
    pos: usize,
    ctx: &'static str,
}

impl<'a> WireReader<'a> {
    /// Create a reader over `buf`. `ctx` labels the packet/struct for panic diagnostics
    /// (e.g. `"OP_TaskDescription"`).
    #[inline]
    pub fn new(buf: &'a [u8], ctx: &'static str) -> Self {
        WireReader { buf, pos: 0, ctx }
    }

    /// Current read offset from the start of the buffer.
    #[inline]
    pub fn pos(&self) -> usize { self.pos }

    /// Total buffer length.
    #[inline]
    pub fn len(&self) -> usize { self.buf.len() }

    /// Bytes not yet consumed.
    #[inline]
    pub fn remaining(&self) -> usize { self.buf.len().saturating_sub(self.pos) }

    /// True when at least `n` bytes remain to be read.
    #[inline]
    pub fn has(&self, n: usize) -> bool { self.remaining() >= n }

    /// True when the cursor has consumed the whole buffer.
    #[inline]
    pub fn at_end(&self) -> bool { self.pos >= self.buf.len() }

    /// True when the buffer is empty.
    #[inline]
    pub fn is_empty(&self) -> bool { self.buf.is_empty() }

    /// Panic helper: called when a required read needs `need` bytes but fewer remain.
    #[cold]
    #[inline(never)]
    fn overrun(&self, what: &str, need: usize) -> ! {
        panic!(
            "wire[{ctx}]: required {what} read at offset {pos} needs {need} byte(s) but only \
             {rem} remain (buffer {len} bytes) — refusing to decode garbage",
            ctx = self.ctx, pos = self.pos, need = need, rem = self.remaining(), len = self.buf.len()
        );
    }

    // ── Required (panicking) integer reads ─────────────────────────────────────

    /// Read a `u8`. **Panics** if the buffer is exhausted.
    #[inline]
    pub fn u8(&mut self) -> u8 {
        match self.try_u8() { Some(v) => v, None => self.overrun("u8", 1) }
    }
    /// Read an `i8`. **Panics** if the buffer is exhausted.
    #[inline]
    pub fn i8(&mut self) -> i8 { self.u8() as i8 }

    /// Read a little-endian `u16`. **Panics** on overrun.
    #[inline]
    pub fn u16(&mut self) -> u16 {
        match self.try_u16() { Some(v) => v, None => self.overrun("u16", 2) }
    }
    /// Read a little-endian `i16`. **Panics** on overrun.
    #[inline]
    pub fn i16(&mut self) -> i16 { self.u16() as i16 }

    /// Read a little-endian `u32`. **Panics** on overrun.
    #[inline]
    pub fn u32(&mut self) -> u32 {
        match self.try_u32() { Some(v) => v, None => self.overrun("u32", 4) }
    }
    /// Read a little-endian `i32`. **Panics** on overrun.
    #[inline]
    pub fn i32(&mut self) -> i32 { self.u32() as i32 }

    /// Read a little-endian `u64`. **Panics** on overrun.
    #[inline]
    pub fn u64(&mut self) -> u64 {
        match self.try_u64() { Some(v) => v, None => self.overrun("u64", 8) }
    }

    /// Read a little-endian `f32`. **Panics** on overrun.
    #[inline]
    pub fn f32(&mut self) -> f32 { f32::from_bits(self.u32()) }

    /// Read a big-endian `u16`. **Panics** on overrun.
    #[inline]
    pub fn u16_be(&mut self) -> u16 {
        match self.try_u16_be() { Some(v) => v, None => self.overrun("u16(be)", 2) }
    }
    /// Read a big-endian `u32`. **Panics** on overrun.
    #[inline]
    pub fn u32_be(&mut self) -> u32 {
        match self.try_u32_be() { Some(v) => v, None => self.overrun("u32(be)", 4) }
    }

    // ── Required (panicking) string / skip ─────────────────────────────────────

    /// Read a NUL-terminated string (lossy UTF-8), advancing past the terminator. **Panics** if no
    /// NUL is found before the end of the buffer (a required string field must be terminated).
    #[inline]
    pub fn cstr(&mut self) -> String {
        match self.try_cstr() { Some(s) => s, None => self.overrun("cstr (no NUL terminator)", self.remaining().max(1)) }
    }

    /// Read a fixed `n`-byte field as a string, stopping at the first embedded NUL. Advances by
    /// exactly `n`. **Panics** if fewer than `n` bytes remain.
    #[inline]
    pub fn fixed_cstr(&mut self, n: usize) -> String {
        match self.try_fixed_cstr(n) { Some(s) => s, None => self.overrun("fixed_cstr", n) }
    }

    /// Skip `n` bytes. **Panics** if fewer than `n` bytes remain.
    #[inline]
    pub fn skip(&mut self, n: usize) {
        if self.remaining() < n { self.overrun("skip", n); }
        self.pos += n;
    }

    /// Borrow the next `n` bytes as a slice, advancing past them. **Panics** if fewer than `n`
    /// bytes remain.
    #[inline]
    pub fn bytes(&mut self, n: usize) -> &'a [u8] {
        if self.remaining() < n { self.overrun("bytes", n); }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        s
    }

    // ── Non-panicking (optional / variable-length) reads ───────────────────────

    /// Borrow the remaining (unconsumed) bytes without advancing.
    #[inline]
    pub fn rest(&self) -> &'a [u8] { &self.buf[self.pos.min(self.buf.len())..] }

    /// Peek the next byte without consuming it. `None` at end of buffer.
    #[inline]
    pub fn peek_u8(&self) -> Option<u8> { self.buf.get(self.pos).copied() }

    /// Try to read a `u8`; `None` if exhausted.
    #[inline]
    pub fn try_u8(&mut self) -> Option<u8> {
        let v = *self.buf.get(self.pos)?;
        self.pos += 1;
        Some(v)
    }

    /// Try to read a little-endian `u16`; `None` if fewer than 2 bytes remain.
    #[inline]
    pub fn try_u16(&mut self) -> Option<u16> {
        let b = self.buf.get(self.pos..self.pos + 2)?;
        self.pos += 2;
        Some(u16::from_le_bytes([b[0], b[1]]))
    }

    /// Try to read a little-endian `u32`; `None` if fewer than 4 bytes remain.
    #[inline]
    pub fn try_u32(&mut self) -> Option<u32> {
        let b = self.buf.get(self.pos..self.pos + 4)?;
        self.pos += 4;
        Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Try to read a little-endian `i32`; `None` if fewer than 4 bytes remain.
    #[inline]
    pub fn try_i32(&mut self) -> Option<i32> { self.try_u32().map(|v| v as i32) }

    /// Try to read a little-endian `u64`; `None` if fewer than 8 bytes remain.
    #[inline]
    pub fn try_u64(&mut self) -> Option<u64> {
        let b = self.buf.get(self.pos..self.pos + 8)?;
        self.pos += 8;
        Some(u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
    }

    /// Try to read a little-endian `f32`; `None` if fewer than 4 bytes remain.
    #[inline]
    pub fn try_f32(&mut self) -> Option<f32> { self.try_u32().map(f32::from_bits) }

    /// Try to read a big-endian `u16`; `None` if fewer than 2 bytes remain.
    #[inline]
    pub fn try_u16_be(&mut self) -> Option<u16> {
        let b = self.buf.get(self.pos..self.pos + 2)?;
        self.pos += 2;
        Some(u16::from_be_bytes([b[0], b[1]]))
    }

    /// Try to read a big-endian `u32`; `None` if fewer than 4 bytes remain.
    #[inline]
    pub fn try_u32_be(&mut self) -> Option<u32> {
        let b = self.buf.get(self.pos..self.pos + 4)?;
        self.pos += 4;
        Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Try to skip `n` bytes; `None` (and no advance) if fewer than `n` remain.
    #[inline]
    pub fn try_skip(&mut self, n: usize) -> Option<()> {
        if self.remaining() < n { return None; }
        self.pos += n;
        Some(())
    }

    /// Try to read a NUL-terminated string (lossy UTF-8), advancing past the terminator. `None`
    /// (and no advance) if there is no NUL in the remaining buffer.
    #[inline]
    pub fn try_cstr(&mut self) -> Option<String> {
        let rel = self.buf.get(self.pos..)?.iter().position(|&b| b == 0)?;
        let end = self.pos + rel;
        let s = String::from_utf8_lossy(&self.buf[self.pos..end]).into_owned();
        self.pos = end + 1; // consume the NUL
        Some(s)
    }

    /// Try to read a NUL-terminated string, but replace it with an empty string when the bytes are
    /// not all printable ASCII (`0x20..0x7f`). Mirrors the RoF2 spawn parser's name/last-name
    /// sanitisation. `None` (no advance) if there is no NUL. Consumes the NUL on success.
    #[inline]
    pub fn try_cstr_ascii(&mut self) -> Option<String> {
        let rel = self.buf.get(self.pos..)?.iter().position(|&b| b == 0)?;
        let end = self.pos + rel;
        let raw = &self.buf[self.pos..end];
        let s = if raw.iter().all(|&b| (0x20..0x7f).contains(&b)) {
            String::from_utf8_lossy(raw).into_owned()
        } else {
            String::new()
        };
        self.pos = end + 1;
        Some(s)
    }

    /// Try to read a fixed `n`-byte string field (string truncated at first NUL). `None` (no
    /// advance) if fewer than `n` bytes remain.
    #[inline]
    pub fn try_fixed_cstr(&mut self, n: usize) -> Option<String> {
        let slice = self.buf.get(self.pos..self.pos + n)?;
        let nul = slice.iter().position(|&b| b == 0).unwrap_or(slice.len());
        let s = String::from_utf8_lossy(&slice[..nul]).into_owned();
        self.pos += n;
        Some(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_scalars_in_order() {
        // u8=0x11, u16=0x2233, u32=0x44556677, f32=1.5
        let mut b = vec![0x11u8, 0x33, 0x22, 0x77, 0x66, 0x55, 0x44];
        b.extend_from_slice(&1.5f32.to_le_bytes());
        let mut r = WireReader::new(&b, "test");
        assert_eq!(r.u8(), 0x11);
        assert_eq!(r.u16(), 0x2233);
        assert_eq!(r.u32(), 0x4455_6677);
        assert_eq!(r.f32(), 1.5);
        assert!(r.at_end());
    }

    #[test]
    fn reads_cstr_and_fixed_cstr() {
        let mut b = b"Hi\0".to_vec();
        b.extend_from_slice(b"Name\0\0\0\0"); // 8-byte fixed field "Name"
        let mut r = WireReader::new(&b, "test");
        assert_eq!(r.cstr(), "Hi");
        assert_eq!(r.fixed_cstr(8), "Name");
        assert!(r.at_end());
    }

    #[test]
    fn big_endian_reads() {
        let b = [0x12u8, 0x34, 0xAA, 0xBB, 0xCC, 0xDD];
        let mut r = WireReader::new(&b, "test");
        assert_eq!(r.u16_be(), 0x1234);
        assert_eq!(r.u32_be(), 0xAABB_CCDD);
    }

    /// (a) A truncated REQUIRED read panics with the diagnostic.
    #[test]
    #[should_panic(expected = "wire[OP_Test]: required u32 read")]
    fn required_read_past_end_panics() {
        let b = [0x01u8, 0x02]; // only 2 bytes
        let mut r = WireReader::new(&b, "OP_Test");
        r.u32(); // needs 4 → panic
    }

    /// A required cstr with no NUL terminator panics.
    #[test]
    #[should_panic(expected = "no NUL terminator")]
    fn required_cstr_without_nul_panics() {
        let b = b"unterminated".to_vec();
        let mut r = WireReader::new(&b, "OP_Test");
        r.cstr();
    }

    /// (c) An OPTIONAL / variable-length trailing field that is ABSENT does NOT panic — the
    /// non-panicking path returns None / reports no remaining bytes.
    #[test]
    fn optional_trailing_field_absent_does_not_panic() {
        let b = [0x01u8, 0x00, 0x00, 0x00]; // exactly one u32, no optional trailer
        let mut r = WireReader::new(&b, "OP_Test");
        assert_eq!(r.u32(), 1);
        assert!(r.at_end());
        assert!(!r.has(4));
        assert_eq!(r.try_u32(), None, "absent optional trailing u32 → None, no panic");
        assert_eq!(r.try_cstr(), None, "absent optional trailing cstr → None, no panic");
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn has_and_remaining_track_position() {
        let b = [0u8; 10];
        let mut r = WireReader::new(&b, "test");
        assert!(r.has(10));
        r.skip(6);
        assert_eq!(r.remaining(), 4);
        assert!(r.has(4) && !r.has(5));
    }

    #[test]
    fn ascii_cstr_sanitises_non_printable() {
        let b = [0x41u8, 0x00, 0x01, 0x02, 0x00]; // "A\0" then non-printable "\x01\x02\0"
        let mut r = WireReader::new(&b, "test");
        assert_eq!(r.try_cstr_ascii(), Some("A".to_string()));
        assert_eq!(r.try_cstr_ascii(), Some(String::new()), "non-printable bytes → empty string");
    }
}
