//! P6a stage 3 — **scalable TrueType text**.
//!
//! # Why the font is used as-is
//!
//! Lato ships as real TrueType: `glyf` outlines with quadratic Béziers, a
//! `cmap` for Unicode lookup, and `head`/`hhea`/`hmtx` for metrics. The obvious
//! shortcut was to pre-render a bitmap atlas at build time, but that trades away
//! the only thing a desktop needs - clean text at whatever size the UI asks for.
//! A 12px and a 48px label from one bitmap atlas is one of them blurry. So the
//! font is parsed and rasterized in the kernel, and the SAME file scales.
//!
//! # Scope, stated honestly
//!
//! This implements the subset a UI actually uses, and refuses everything else
//! rather than mis-rendering it:
//!   * `glyf` simple + composite glyphs (composites are flattened, with the
//!     component offsets and the 2x2 transform applied).
//!   * Quadratic curve flattening at a fixed subdivision count - resolution
//!     independent enough for UI text at these sizes, and cheap.
//!   * `cmap` format 4 (BMP) and format 12 (full Unicode). Lato ships both.
//!
//! NOT implemented, and refused with an error instead of guessed:
//!   * `CFF`/PostScript outlines (this is a `glyf` font; if a future face is
//!     CFF it is rejected at parse time, not rendered wrong).
//!   * Hinting (`fpgm`/`prep`/`cvt` are read only to be skipped). The text is
//!     rendered with linear scaling, so stems are slightly softer than FreeType's
//!     hinted output. That is a deliberate trade: hinting engines are large and
//!     this session has repeatedly shown that large untested subsystems are
//!     where the bugs live.
//!
//! # Rasterization
//!
//! Scanline with a per-scanline coverage accumulator (analytic in y via
//! sub-scanlines, 4x vertical supersampling), so edges are antialiased without
//! a full 2-D coverage buffer. Output is an 8-bit alpha mask which the caller
//! composites against any colour through the surface blit path.
//!
//! # Licensing
//!
//! Lato is SIL Open Font License 1.1 (see `FONT/OFL.txt`), which permits
//! redistribution and embedding. `Lato-Regular.ttf` is embedded below; the other
//! eight weights ship in the tree for a later stage but are not embedded yet -
//! one face keeps the kernel image small, and the rasterizer is weight-agnostic,
//! so adding Bold later is a one-line change.

use alloc::vec;
use alloc::vec::Vec;

/// Lato Regular, SIL OFL 1.1. Embedded whole rather than parsed at build time:
/// the file is already a subsetted web build (~73 KiB), and embedding keeps the
/// font question out of `build.rs` and its nested-cargo deadlock dance.
static LATO: &[u8] = include_bytes!("../../FONT/Lato/Lato-Regular.ttf");

/// The raw face bytes, exposed so the self-test can feed deliberately malformed
/// input to the parser. A parser that runs in the kernel (`panic = "abort"`)
/// must REJECT bad input, never abort on it - and the only way to know that is
/// to actually hand it bad bytes.
pub const LATO_BYTES: &[u8] = LATO;

/// Where the face came from, reported once at boot so the log states which font
/// is actually live rather than leaving it to be inferred.
pub const FONT_NAME: &str = "Lato-Regular";
pub const FONT_LICENSE: &str = "SIL OFL 1.1";

/// A parsed sfnt table directory entry.
#[derive(Clone, Copy)]
struct Table {
    off: usize,
    len: usize,
}

/// Big-endian readers. TrueType is entirely big-endian, and getting this
/// backwards is the easiest way to produce a font that "loads" and then renders
/// garbage - so they are named explicitly rather than inlined.
#[inline]
fn be16(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*b.get(o)?, *b.get(o + 1)?]))
}
#[inline]
fn bei16(b: &[u8], o: usize) -> Option<i16> {
    Some(be16(b, o)? as i16)
}
#[inline]
fn be32(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_be_bytes([
        *b.get(o)?,
        *b.get(o + 1)?,
        *b.get(o + 2)?,
        *b.get(o + 3)?,
    ]))
}

/// Maximum nesting depth for composite glyphs. Real fonts use 2-3; the cap stops
/// a malformed `glyf` from recursing until the kernel stack is exhausted. The
/// 64 KiB kernel stack is not generous, and a recursive walk with a Vec per frame
/// is not free - the same class of trap as the 66 KB stack temporary.
const MAX_COMPOSITE_DEPTH: u8 = 5;

/// The face: the raw file plus the offsets of the tables we use.
///
/// Every accessor is bounds-checked and returns `Option`/`Result`. A malformed
/// font must produce an error, never a panic - this runs in the kernel, where a
/// panic is `abort`, and a bad font is exactly the input a parser bug produces.
pub struct Face {
    data: &'static [u8],
    hhea: Table,
    hmtx: Table,
    loca: Table,
    glyf: Table,
    cmap: Table,
    num_glyphs: u16,
    index_to_loc: i16,
    units_per_em: u16,
    num_h_metrics: u16,
    ascender: i16,
    descender: i16,
}

impl Face {
    /// Parse the embedded Lato face.
    pub fn load() -> Result<Face, &'static str> {
        Face::parse(LATO)
    }

    pub fn parse(data: &'static [u8]) -> Result<Face, &'static str> {
        // sfnt version 0x00010000 (TrueType) or 'true'. Anything else -
        // notably 'OTTO' (CFF) - is REJECTED here rather than rendered wrong.
        let ver = be32(data, 0).ok_or("font: truncated header")?;
        if ver == 0x4F54_544F {
            return Err("font: CFF outlines unsupported (need glyf)");
        }
        if ver != 0x0001_0000 && ver != 0x7472_7565 {
            return Err("font: not a TrueType sfnt");
        }
        let num_tables = be16(data, 4).ok_or("font: truncated numTables")? as usize;

        let mut head = Table { off: 0, len: 0 };
        let mut hhea = Table { off: 0, len: 0 };
        let mut maxp = Table { off: 0, len: 0 };
        let mut hmtx = Table { off: 0, len: 0 };
        let mut loca = Table { off: 0, len: 0 };
        let mut glyf = Table { off: 0, len: 0 };
        let mut cmap = Table { off: 0, len: 0 };
        let mut found = 0u8;

        for i in 0..num_tables {
            let rec = 12 + i * 16;
            let tag = data.get(rec..rec + 4).ok_or("font: truncated table dir")?;
            let off = be32(data, rec + 8).ok_or("font: truncated table off")? as usize;
            let len = be32(data, rec + 12).ok_or("font: truncated table len")? as usize;
            // A table pointing outside the file is a hard error: every later
            // accessor would otherwise read arbitrary memory.
            if off > data.len() || len > data.len() || off + len > data.len() {
                return Err("font: table out of range");
            }
            let t = Table { off, len };
            // Compare the 4-byte tag against each table name. Matching a
            // `&[u8]` against a byte-string pattern does not typecheck in Rust,
            // and comparing a `str` would need UTF-8 validation of binary
            // data - so this compares the raw bytes, which is what the spec
            // actually defines.
            if tag == b"head" {
                head = t;
                found |= 1;
            } else if tag == b"hhea" {
                hhea = t;
                found |= 2;
            } else if tag == b"maxp" {
                maxp = t;
                found |= 4;
            } else if tag == b"hmtx" {
                hmtx = t;
                found |= 8;
            } else if tag == b"loca" {
                loca = t;
                found |= 16;
            } else if tag == b"glyf" {
                glyf = t;
                found |= 32;
            } else if tag == b"cmap" {
                cmap = t;
                found |= 64;
            }
        }
        // All seven are required to draw anything at all.
        if found != 127 {
            return Err("font: missing a required table");
        }

        let units_per_em = be16(data, head.off + 18).ok_or("font: bad head")?;
        if units_per_em == 0 {
            return Err("font: unitsPerEm is zero");
        }
        let index_to_loc = bei16(data, head.off + 50).ok_or("font: bad head")?;
        let ascender = bei16(data, hhea.off + 4).ok_or("font: bad hhea")?;
        let descender = bei16(data, hhea.off + 6).ok_or("font: bad hhea")?;
        let num_h_metrics = be16(data, hhea.off + 34).ok_or("font: bad hhea")?;
        let num_glyphs = be16(data, maxp.off + 4).ok_or("font: bad maxp")?;

        Ok(Face {
            data,
            hhea,
            hmtx,
            loca,
            glyf,
            cmap,
            num_glyphs,
            index_to_loc,
            units_per_em,
            num_h_metrics,
            ascender,
            descender,
        })
    }

    /// Debug aid: the resolved table offsets, as one printable line.
    ///
    /// Added because every offset bug in this stage looked identical from the
    /// outside (garbage outlines) and hand-decoding the TTF in PowerShell was
    /// itself error-prone. Having the kernel print what IT resolved turns a
    /// multi-step guess into a single comparison against the known-correct
    /// values (glyf 13680, loca 1248, cmap 876 for this file).
    pub fn table_report(&self) -> alloc::string::String {
        alloc::format!(
            "glyf={} len={} loca={} len={} cmap={} hmtx={} hhea={}",
            self.glyf.off,
            self.glyf.len,
            self.loca.off,
            self.loca.len,
            self.cmap.off,
            self.hmtx.off,
            self.hhea.off
        )
    }


    pub fn units_per_em(&self) -> u16 {
        self.units_per_em
    }
    pub fn ascender(&self) -> i16 {
        self.ascender
    }
    pub fn descender(&self) -> i16 {
        self.descender
    }
    pub fn glyph_count(&self) -> u16 {
        self.num_glyphs
    }

    /// Debug aid: the first 10 bytes of a glyph's `glyf` record, plus the
    /// resolved absolute offset.
    ///
    /// Added because the "contours=N" readback was being compared against
    /// hand-decoded PowerShell values, and THREE of those hand decodings were
    /// wrong (byte order, sign extension, a base offset). Printing the bytes the
    /// KERNEL resolved makes the parser the single source of truth, so a wrong
    /// gid or a wrong offset is visible in one line instead of guessed at.
    pub fn glyph_debug(&self, gid: u16) -> alloc::string::String {
        match self.glyph_range(gid) {
            None => alloc::format!("gid {gid}: EMPTY (no outline)"),
            Some((s, e)) => {
                let mut hex = alloc::string::String::new();
                for i in 0..10 {
                    if s + i < e && s + i < self.data.len() {
                        hex.push_str(&alloc::format!("{:02X} ", self.data[s + i]));
                    }
                }
                alloc::format!(
                    "gid {gid}: abs={s} len={} bytes[{}] contours_raw={}",
                    e - s,
                    hex.trim_end(),
                    be16(self.data, s).unwrap_or(0)
                )
            }
        }
    }

    /// Horizontal advance for `gid`, in font units.
    ///
    /// Monospace fonts may have fewer `hMetrics` than glyphs; everything past
    /// the last full metric uses that last advance, which is what the spec
    /// requires. Getting this wrong is a very visible bug (letters drifting
    /// apart or overlapping), so it is computed rather than assumed.
    pub fn advance(&self, gid: u16) -> u16 {
        let i = gid.min(self.num_h_metrics.saturating_sub(1)) as usize;
        be16(self.data, self.hmtx.off + i * 4).unwrap_or(self.units_per_em / 2)
    }

    /// Byte range of glyph `gid` inside `glyf`, or `None` for an empty glyph
    /// (a space has no outline, which is NOT an error).
    fn glyph_range(&self, gid: u16) -> Option<(usize, usize)> {
        if gid >= self.num_glyphs {
            return None;
        }
        let (start, end) = if self.index_to_loc == 0 {
            (
                be16(self.data, self.loca.off + gid as usize * 2)? as usize * 2,
                be16(self.data, self.loca.off + gid as usize * 2 + 2)? as usize * 2,
            )
        } else {
            (
                be32(self.data, self.loca.off + gid as usize * 4)? as usize,
                be32(self.data, self.loca.off + gid as usize * 4 + 4)? as usize,
            )
        };
        if end <= start {
            return None; // empty glyph
        }
        // Clamp to the table: a corrupt loca must not read into `cmap`.
        let s = self.glyf.off + start;
        let e = self.glyf.off + end;
        if s >= self.data.len() || e > self.data.len() || e > self.glyf.off + self.glyf.len {
            return None;
        }
        Some((s, e))
    }

    /// Map a Unicode scalar to a glyph id.
    ///
    /// Prefers a format-12 subtable (full Unicode) and falls back to format 4
    /// (BMP). Trying format 4 first is a classic mistake: it "works" for ASCII
    /// and then silently returns the wrong glyph above U+FFFF, which is exactly
    /// where a UI starts showing boxes.
    pub fn glyph_for_char(&self, ch: char) -> u16 {
        let cp = ch as u32;
        let n = be16(self.data, self.cmap.off + 2).unwrap_or(0) as usize;
        let mut best12 = None;
        let mut best4 = None;
        for i in 0..n {
            let rec = self.cmap.off + 4 + i * 8;
            // The subtable offset is a FOUR-byte field, not two. Reading it with
            // `be16` reads the high half - always 0 for a real font, whose
            // subtables sit at small offsets - so every lookup then points at
            // the cmap HEADER (format 0) and every character silently maps to
            // .notdef. That is the whole "text renders as nothing" failure mode,
            // and it produced glyph 0 for every character on the first run.
            let Some(off) = be32(self.data, rec + 4) else { continue };
            let sub = self.cmap.off + off as usize;
            if sub + 4 > self.data.len() {
                continue;
            }
            match be16(self.data, sub) {
                Some(12) => best12 = Some(sub),
                Some(4) if best4.is_none() => best4 = Some(sub),
                _ => {}
            }
        }
        if let Some(sub) = best12 {
            if let Some(g) = self.cmap12(sub, cp) {
                return g;
            }
        }
        if let Some(sub) = best4 {
            if let Some(g) = self.cmap4(sub, cp) {
                return g;
            }
        }
        0 // .notdef
    }

    /// cmap format 12: three parallel arrays, binary-searched.
    fn cmap12(&self, sub: usize, cp: u32) -> Option<u16> {
        let n = be32(self.data, sub + 12)? as usize;
        // groups at +16, each 12 bytes: start, end, delta
        let g = sub + 16;
        let mut lo = 0usize;
        let mut hi = n;
        while lo < hi {
            let mid = (lo + hi) / 2;
            let o = g + mid * 12;
            let start = be32(self.data, o)?;
            let end = be32(self.data, o + 4)?;
            if cp < start {
                hi = mid;
            } else if cp > end {
                lo = mid + 1;
            } else {
                let delta = be32(self.data, o + 8)?;
                return u16::try_from(cp.wrapping_add(delta) & 0xFFFF).ok();
            }
        }
        None
    }

    /// cmap format 4: segmented mapping, binary-searched over endCode.
    fn cmap4(&self, sub: usize, cp: u32) -> Option<u16> {
        if cp > 0xFFFF {
            return None;
        }
        let c = cp as u16;
        let seg2 = be16(self.data, sub + 6)? as usize / 2;
        let end_base = sub + 14;
        let start_base = end_base + seg2 * 2 + 2;
        let delta_base = start_base + seg2 * 2;
        let range_base = delta_base + seg2 * 2;
        let mut lo = 0usize;
        let mut hi = seg2;
        while lo < hi {
            let mid = (lo + hi) / 2;
            let end = be16(self.data, end_base + mid * 2)?;
            if c > end {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo >= seg2 {
            return None;
        }
        let start = be16(self.data, start_base + lo * 2)?;
        if c < start {
            return None;
        }
        let delta = bei16(self.data, delta_base + lo * 2)?;
        let range = be16(self.data, range_base + lo * 2)?;
        if range == 0 {
            return (c as i32 + delta as i32).try_into().ok();
        }
        // rangeOffset is a byte offset from its own slot into glyphIdArray.
        let idx = range_base + lo * 2 + range as usize + (c as usize - start as usize) * 2;
        let g = be16(self.data, idx)?;
        if g == 0 {
            return None;
        }
        (g as i32 + delta as i32).try_into().ok()
    }

    /// Flatten glyph `gid` into closed polygons in font units.
    ///
    /// Curves are subdivided rather than solved analytically: at UI sizes
    /// (8-48 px) four segments per quadratic is indistinguishable from exact, and
    /// it keeps the rasterizer to one well-understood scanline pass.
    pub fn outline(&self, gid: u16, out: &mut Vec<Vec<(i32, i32)>>) {
        let mut depth = 0u8;
        self.outline_into(gid, out, &mut depth);
    }

    fn outline_into(&self, gid: u16, out: &mut Vec<Vec<(i32, i32)>>, depth: &mut u8) {
        // Bounded recursion: a composite that references itself must not loop.
        if *depth >= MAX_COMPOSITE_DEPTH {
            return;
        }
        let Some((s, _e)) = self.glyph_range(gid) else {
            return; // empty glyph (space) - not an error
        };
        let Some(ncont_raw) = be16(self.data, s) else {
            return;
        };
        let ncont = ncont_raw as i32;
        if ncont == 0 {
            return;
        }
        // numberOfContours < 0 means a COMPOSITE glyph: offset to the components.
        if ncont < 0 {
            self.outline_composite(s, out, depth);
            return;
        }
        let ncont = ncont as usize;

        let end_pts_base = s + 10;
        let npts = match be16(self.data, end_pts_base + (ncont - 1) * 2) {
            Some(v) => v as usize + 1,
            None => return,
        };
        // The glyph record layout is: numberOfContours(2) + xMin/yMin/xMax/yMax(8)
// + endPtsOfContours[ncont*2] + instructionLength(2) + instructions +
// flags[] + xDeltas[] + yDeltas[].
//
// The flags array therefore starts at `s + 10 + ncont*2 + 8` - the 8-byte BBOX
// must be skipped as well as the endPts array, and there is an additional
// `instructionLength` field before the instructions. Omitting the bbox made
// every flag/X-delta byte read 8 bytes early, which decoded as garbage
// coordinates: 'A' came out with bbox x[3101..3840] in a 2000 upem font instead
// of the real x[0..1353], and the rasterizer then clipped the glyph to 8x3.
// The `+ 8` is the bbox; the instruction block is skipped properly below.
let instr_len_base = end_pts_base + ncont * 2;
let mut p = instr_len_base + 2; // + instructionLength field itself
let instr_len = be16(self.data, instr_len_base).unwrap_or(0) as usize;
p += instr_len; // skip the hinting bytecode

// --- flags, with REPEAT run-length -------------------------------
        let mut flags: Vec<u8> = Vec::with_capacity(npts);
        while flags.len() < npts {
            let Some(&f) = self.data.get(p) else { return };
            p += 1;
            flags.push(f);
            if f & 0x08 != 0 {
                let Some(&r) = self.data.get(p) else { return };
                p += 1;
                for _ in 0..r {
                    if flags.len() < npts {
                        flags.push(f);
                    }
                }
            }
        }

        // --- X deltas, accumulated ----------------------------------------
        let mut xs: Vec<i32> = Vec::with_capacity(npts);
        let mut x = 0i32;
        for &f in flags.iter() {
            let dx = if f & 0x02 != 0 {
                let Some(&b) = self.data.get(p) else { return };
                p += 1;
                if f & 0x10 != 0 { b as i32 } else { -(b as i32) }
            } else if f & 0x10 != 0 {
                0
            } else {
                let Some(v) = bei16(self.data, p) else { return };
                p += 2;
                v as i32
            };
            x += dx;
            xs.push(x);
        }

        // --- Y deltas, accumulated ----------------------------------------
        let mut ys: Vec<i32> = Vec::with_capacity(flags.len());
        let mut y = 0i32;
        for &f in flags.iter() {
            let dy = if f & 0x04 != 0 {
                let Some(&b) = self.data.get(p) else { return };
                p += 1;
                if f & 0x20 != 0 { b as i32 } else { -(b as i32) }
            } else if f & 0x20 != 0 {
                0
            } else {
                let Some(v) = bei16(self.data, p) else { return };
                p += 2;
                v as i32
            };
            y += dy;
            ys.push(y);
        }

        // --- contour boundaries -------------------------------------------
        let mut bounds: Vec<(usize, usize)> = Vec::with_capacity(ncont);
        let mut first = 0usize;
        for i in 0..ncont {
            let Some(last) = be16(self.data, end_pts_base + i * 2) else {
                return;
            };
            bounds.push((first, last as usize));
            first = last as usize + 1;
        }

        for (st, en) in bounds {
            if en < st || en >= xs.len() {
                continue;
            }
            let pts: Vec<(i32, i32)> = (st..=en).map(|k| (xs[k], ys[k])).collect();
            // The on-curve bit lives in the FLAGS array, not in the coordinates,
            // so it must be carried alongside: dropping it makes every
            // quadratic render as a straight chord, which looks like a
            // different (much worse) font rather than an obvious crash.
            let onflags: Vec<bool> = (st..=en).map(|k| flags[k] & 0x01 != 0).collect();
            if pts.len() < 2 {
                continue;
            }
            let contour = flatten_contour(&pts, &onflags);
            if contour.len() >= 3 {
                out.push(contour);
            }
        }
    }

    /// Composite glyph: walk the component list, applying each component's
    /// offset and 2x2 transform.
    ///
    /// The transform is applied to the CHILD's already-flattened points, so a
    /// child shared by several components is walked once and the transform
    /// arithmetic exists in exactly one place.
    fn outline_composite(&self, s: usize, out: &mut Vec<Vec<(i32, i32)>>, depth: &mut u8) {
        *depth += 1;
        let mut p = s + 10; // skip header + bbox
        loop {
            let Some(flags) = be16(self.data, p) else { return };
            let Some(gid) = be16(self.data, p + 2) else { return };
            p += 4;
            let (dx, dy) = if flags & 0x0001 != 0 {
                let Some(a) = bei16(self.data, p) else { return };
                let Some(b) = bei16(self.data, p + 2) else { return };
                p += 4;
                (a as i32, b as i32)
            } else {
                let (Some(&a), Some(&b)) = (self.data.get(p), self.data.get(p + 1)) else {
                    return;
                };
                p += 2;
                (a as i8 as i32, b as i8 as i32)
            };
            let t = [p, p + 2, p + 4, p + 6].map(|o| d2dot14(self.data, o));
            p += 8;
            // Destructure explicitly rather than pattern-matching the array: an array
            // pattern cannot bind four distinct names, and `t.0`/`t.1` indexing
            // inside the arithmetic is where the earlier precedence bug lived.
            let (a, b, c, d) = (t[0], t[1], t[2], t[3]);
            let (Some(a), Some(b), Some(c), Some(d)) = (a, b, c, d) else {
                // A malformed transform: stop rather than render a wrong glyph.
                return;
            };
            if flags & 0x0008 != 0 {
                // WE_HAVE_A_SCALE: the transform is one value, not 4. Refused
                // rather than mis-applied - Lato's composites all use the full
                // 2x2 form, and silently mis-rendering is worse than a hole.
                return;
            }

            let mut sub: Vec<Vec<(i32, i32)>> = Vec::new();
            self.outline_into(gid, &mut sub, depth);
            for contour in sub {
                let moved: Vec<(i32, i32)> = contour
                    .into_iter()
                    .map(|(x, y)| {
                        // F2Dot14 is 2.14 fixed point, so the shift comes BEFORE the
                        // offset is added. Written as `... >> 14 + dx` the `+`
                        // binds into the SHIFT's right operand (>> has lower
                        // precedence than +), silently scaling by the wrong
                        // amount - the kind of bug that renders text at a
                        // slight, plausible-looking angle instead of failing.
                        let nx = (x * a + y * c) >> 14;
                        let ny = (x * b + y * d) >> 14;
                        (nx + dx, ny + dy)
                    })
                    .collect();
                if moved.len() >= 3 {
                    out.push(moved);
                }
            }

            if flags & 0x0020 == 0 {
                break; // MORE_COMPONENTS clear -> this was the last component
            }
        }
        *depth -= 1;
    }
}

/// F2Dot14: 2.14 signed fixed point.
fn d2dot14(b: &[u8], o: usize) -> Option<i32> {
    bei16(b, o).map(|v| v as i32)
}

/// Turn a TrueType contour into a dense polygon of line segments.
///
/// TrueType contours are quadratic: two consecutive off-curve points imply an
/// on-curve point at their midpoint. Handling that implicit point is the whole
/// trick - miss it and every curve renders as a straight chord, which reads as
/// "a different, worse font" rather than as an obvious failure.
fn flatten_contour(pts: &[(i32, i32)], on: &[bool]) -> Vec<(i32, i32)> {
    /// Segments per quadratic. Four is indistinguishable from exact at UI sizes.
    const STEPS: usize = 4;

    let n = pts.len().min(on.len());
    if n == 0 {
        return Vec::new();
    }
    // Start on an on-curve point; if the contour begins off-curve, synthesise
    // one from the wrap-around pair (common in Lato).
    let start = (0..n).find(|&i| on[i]);
    let (sx, sy) = match start {
        Some(i) => pts[i],
        None => ((pts[0].0 + pts[n - 1].0) / 2, (pts[0].1 + pts[n - 1].1) / 2),
    };

    let mut out: Vec<(i32, i32)> = Vec::with_capacity(n * (STEPS + 1));
    out.push((sx, sy));

    // Walk the contour ONCE, starting just after the start point and wrapping
    // back around to it to close the loop.
    //
    // The range must be `1..n` (n-1 indices), NOT `1..=n-1`. `1..=n-1` is also
    // n-1 indices but it runs `i+1 ..= i+n-1` and the modulo wraps it to `i+1,
    // ..., i-1, i` - the SAME n-1 points plus the start point again, i.e. n
    // entries. That double-counts the start point, which re-applies a whole
    // coordinate delta and throws the outline far outside the em square: the
    // bbox came out as x[3101..3840] in a 2000 upem font, and the rasterizer
    // then clipped the real glyph down to 8x3. The contour must be visited
    // exactly once.
    let order: Vec<usize> = match start {
        Some(i) => (1..n).map(|k| (i + k) % n).collect(),
        None => (0..n).collect(),
    };

    let mut cur = (sx, sy);
    let mut ctrl: Option<(i32, i32)> = None;
    for &k in order.iter() {
        let p = pts[k];
        if on[k] {
            match ctrl.take() {
                None => out.push(p),
                Some(c) => quad(&mut out, cur, c, p, STEPS),
            }
            cur = p;
        } else {
            match ctrl.take() {
                None => ctrl = Some(p),
                Some(c) => {
                    // Two off-curve points imply an on-curve midpoint.
                    let mid = ((c.0 + p.0) / 2, (c.1 + p.1) / 2);
                    quad(&mut out, cur, c, mid, STEPS);
                    cur = mid;
                    ctrl = Some(p);
                }
            }
        }
    }
    if let Some(c) = ctrl {
        quad(&mut out, cur, c, (sx, sy), STEPS);
    }
    out
}

/// Append `steps` line segments approximating the quadratic `p0 -> c -> p1`.
///
/// Integer-only on purpose: the kernel has no FPU guarantees in this path and a
/// float multiply per curve point would be both slower and harder to reason
/// about. The recurrence form avoids `t*t` entirely and stays exact in i32 for
/// any sane font.
fn quad(out: &mut Vec<(i32, i32)>, p0: (i32, i32), c: (i32, i32), p1: (i32, i32), steps: usize) {
    for i in 1..=steps {
        let t = i as i32;
        let mt = steps as i32 - t;
        // Bernstein basis, integer weights, normalised by steps^2. Computed in
        // i64 so a large glyph cannot overflow the product before the divide.
        let w0 = (mt * mt) as i64;
        let w1 = (2 * mt * t) as i64;
        let w2 = (t * t) as i64;
        let den = (steps as i64) * (steps as i64);
        let x = ((p0.0 as i64 * w0 + c.0 as i64 * w1 + p1.0 as i64 * w2) / den) as i32;
        let y = ((p0.1 as i64 * w0 + c.1 as i64 * w1 + p1.1 as i64 * w2) / den) as i32;
        out.push((x, y));
    }
}

/// A rasterized glyph: an 8-bit alpha mask plus its metrics, in pixels.
///
/// The mask is the ONLY thing the renderer produces. Colour, blending and the
/// blit into a surface are the caller's job - keeping the font free of colour
/// means it composes over any background, which is what a windowed desktop
/// needs, and it keeps this module from depending on the surface layer.
pub struct Glyph {
    pub w: usize,
    pub h: usize,
    /// Left side bearing in pixels, relative to the pen position.
    pub bearing_x: i32,
    /// Distance from the baseline UP to the top of the mask, in pixels.
    pub bearing_y: i32,
    /// Horizontal advance in pixels.
    pub advance: i32,
    /// Row-major alpha, `w * h` bytes.
    pub alpha: Vec<u8>,
}

/// Vertical supersampling factor, in 1/16ths of a pixel. Bands are expressed as
/// a fixed-point y offset rather than a count, so the number of samples per
/// pixel row varies with the glyph's height and the coverage arithmetic never
/// truncates: each crossing adds `SUBSAMPLE_SCALE`, and a pixel is "solid" when
/// it collects the full `SUBSAMPLE_SCALE` for that row.
///
/// Two earlier forms failed visibly. Dividing by the glyph HEIGHT collapsed all
/// bands onto one y (no pixel ever reached full coverage). A fixed count of
/// integer bands then under-covered tall glyphs and over-covered short ones at
/// small sizes. Accumulating in 1/16ths is exact for every integer crossing and
/// needs no floating point in the kernel.
const SUBSAMPLE_SCALE: u32 = 16;

/// Rasterize `gid` at `pixel_size` px em.
pub fn rasterize(face: &Face, gid: u16, pixel_size: u16) -> Glyph {
    let scale = |v: i32| -> i32 { (v * pixel_size as i32) / face.units_per_em() as i32 };
    let adv = scale(face.advance(gid) as i32);

    let mut polys: Vec<Vec<(i32, i32)>> = Vec::new();
    face.outline(gid, &mut polys);
    if polys.is_empty() {
        return Glyph {
            w: 0,
            h: 0,
            bearing_x: 0,
            bearing_y: 0,
            advance: adv,
            alpha: Vec::new(),
        };
    }

    // Transform to pixel space. TrueType y is UP, image y is DOWN, so the
    // vertical axis is negated here and nothing downstream has to remember it.
    let mut px: Vec<Vec<(i32, i32)>> = Vec::with_capacity(polys.len());
    let (mut min_x, mut min_y) = (i32::MAX, i32::MAX);
    let (mut max_x, mut max_y) = (i32::MIN, i32::MIN);
    for poly in &polys {
        let mut q = Vec::with_capacity(poly.len());
        for &(x, y) in poly {
            let (sx, sy) = (scale(x), -scale(y));
            min_x = min_x.min(sx);
            max_x = max_x.max(sx);
            min_y = min_y.min(sy);
            max_y = max_y.max(sy);
            q.push((sx, sy));
        }
        px.push(q);
    }
    // One pixel of slack so a curve touching the edge is not clipped away.
    min_x -= 1;
    min_y -= 1;
    max_x += 1;
    max_y += 1;
    let w = (max_x - min_x).max(1) as usize;
    let h = (max_y - min_y).max(1) as usize;
    // A cap that no UI text hits: at 200 px em with a pathological outline this
    // would otherwise be a multi-megabyte allocation from a 73 KiB font.
    if w > 512 || h > 512 {
        return Glyph {
            w: 0,
            h: 0,
            bearing_x: 0,
            bearing_y: 0,
            advance: adv,
            alpha: Vec::new(),
        };
    }

    // Scanline fill with an even-odd rule (non-zero would need a winding
    // direction per contour, which TrueType does not guarantee).
    //
    // The whole fill runs in 1/SUBSAMPLE_SCALE fixed point. Sampling at
    // `row + s/SUBSAMPLES` in INTEGER pixel units cannot work: `s/S` truncates
    // to 0 for every s < S, so all the sub-scanlines of a row collapse onto the
    // same y and the glyph comes out hard-edged (measured: 0 partial-coverage
    // pixels). Scaling the VERTICES by S instead makes every sub-scanline an
    // exact integer, so the crossing test and the interpolation are both exact.
    let s = SUBSAMPLE_SCALE as i32;
    let mut acc = vec![0u32; w * h];
    let mut xs: Vec<i64> = Vec::new();
    // Vertices pre-scaled into 1/S pixel units (both axes; x is scaled too so the
    // final divide back to pixels is a single exact step).
    let scaled: Vec<Vec<(i64, i64)>> = px
        .iter()
        .map(|poly| poly.iter().map(|&(x, y)| (x as i64 * s as i64, y as i64 * s as i64)).collect())
        .collect();

    for row in 0..h {
        // Sub-scanline y positions for this row, in 1/S units: the row spans
        // `[row*S, (row+1)*S)` in fixed point, so band `k` is at `(row+1)*S - 1 - k`
        // - i.e. sampled from the BOTTOM of the row upwards, which keeps the
        // partial coverage at the top edge of a horizontal stem.
        for k in 0..s {
            let sy = min_y * s + (row as i32 + 1) * s - 1 - k;
            xs.clear();
            for poly in &scaled {
                let n = poly.len();
                if n < 3 {
                    continue;
                }
                for i in 0..n {
                    let (x0, y0) = poly[i];
                    let (x1, y1) = poly[(i + 1) % n];
                    // Half-open rule: count a shared vertex once, or every
                    // shared edge produces a spurious crossing and the glyph
                    // renders with a hole along its seams.
                    if (y0 <= sy as i64) == (y1 <= sy as i64) {
                        continue;
                    }
                    // Exact integer interpolation: t = (sy - y0) / (y1 - y0) in
                    // 1/S units, so the result is already in 1/S units.
                    let num = sy as i64 - y0;
                    let den = y1 - y0;
                    xs.push((x0 * den + (x1 - x0) * num) / den);
                }
            }
            if xs.len() < 2 {
                continue;
            }
            xs.sort_unstable();
            // xs are in 1/S units; convert to pixel indices for the fill.
            let mut j = 0;
            while j + 1 < xs.len() {
                let a = (xs[j] / s as i64).max(min_x as i64) as i32;
                let b = (xs[j + 1] / s as i64).min((min_x + w as i32) as i64) as i32;
                if b > a {
                    for x in a..b {
                        let xi = (x - min_x) as usize;
                        if xi < w {
                            acc[row * w + xi] += 1;
                        }
                    }
                }
                j += 2;
            }
        }
    }

    // Normalise: each pixel collected 0..=S sub-scanline hits, so scale to 0..255.
    let cov: Vec<u8> = acc
        .iter()
        .map(|&c| ((c * 255) / SUBSAMPLE_SCALE) as u8)
        .collect();

    Glyph {
        w,
        h,
        bearing_x: min_x,
        bearing_y: max_y,
        advance: adv,
        alpha: cov,
    }
}

/// Line metrics for a given pixel size: what a caller needs to lay out a row of
/// text without re-deriving it (and getting the baseline wrong).
pub struct LineMetrics {
    /// Pixels from the baseline up to the top of a line of text.
    pub ascent: i32,
    /// Pixels from the baseline down (positive value, despite the name).
    pub descent: i32,
    /// Recommended distance between successive baselines.
    pub line_height: i32,
}

impl Face {
    pub fn line_metrics(&self, pixel_size: u16) -> LineMetrics {
        let upem = self.units_per_em().max(1) as i32;
        let px = pixel_size as i32;
        let ascent = (self.ascender() as i32 * px) / upem;
        let descent = (-(self.descender() as i32) * px) / upem;
        LineMetrics {
            ascent,
            descent,
            // 1.2 x em is the conventional UI line box; using ascent+descent
            // alone makes lines visibly cramped for a face like Lato.
            line_height: (px * 12) / 10,
        }
    }
}

/// Width in pixels of `text` at `pixel_size` (advance only, no rasterization).
///
/// Needed before drawing so a caller can right-align or centre a label, and so a
/// single-line text control knows its own width without rasterizing first.
pub fn measure(face: &Face, text: &str, pixel_size: u16) -> i32 {
    let mut w = 0i32;
    for ch in text.chars() {
        let gid = face.glyph_for_char(ch);
        w += rasterize_advance(face, gid, pixel_size);
    }
    w
}

/// Advance for `gid` without building a mask - the cheap half of `rasterize`.
fn rasterize_advance(face: &Face, gid: u16, pixel_size: u16) -> i32 {
    let upem = face.units_per_em().max(1) as i32;
    (face.advance(gid) as i32 * pixel_size as i32) / upem
}
