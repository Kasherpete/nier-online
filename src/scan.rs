//! Address resolution over the game image: byte patterns, strings, code references, instruction walks,
//! PE section and `.pdata` lookups, and a hook-site safety check.
//!
//! Everything works on an [`Image`]: the module's bytes laid out as mapped in memory, addressed by RVA
//! (offset from the module base). At runtime that is the live module; in tests it is the exe file mapped
//! by [`map_file`]. Lookup techniques follow AutomataMP `shared/utility/Scan.cpp` (string, reference and
//! disassembly scans) and nier_coop `src/scan.rs` (wildcard patterns, RIP-relative targets).

use std::fmt;

use iced_x86::{Decoder, DecoderOptions, FlowControl, Instruction, OpKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanError(pub String);

impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ScanError {}

pub type Result<T> = std::result::Result<T, ScanError>;

pub fn err<T>(msg: impl Into<String>) -> Result<T> {
    Err(ScanError(msg.into()))
}

// ---------------------------------------------------------------------------------------------------------
// Patterns

/// IDA-style byte pattern: hex bytes separated by spaces, `?` or `??` for a wildcard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pattern(Vec<Option<u8>>);

impl Pattern {
    pub fn parse(s: &str) -> Result<Self> {
        let bytes = s
            .split_whitespace()
            .map(|t| match t {
                "?" | "??" => Ok(None),
                _ => u8::from_str_radix(t, 16).map(Some).map_err(|_| ScanError(format!("bad pattern byte {t:?}"))),
            })
            .collect::<Result<Vec<_>>>()?;
        if bytes.is_empty() {
            return err("empty pattern");
        }
        Ok(Self(bytes))
    }

    /// Exact bytes, no wildcards.
    pub fn exact(bytes: &[u8]) -> Self {
        Self(bytes.iter().copied().map(Some).collect())
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn matches_at(&self, hay: &[u8], at: usize) -> bool {
        hay.get(at..at + self.0.len()).is_some_and(|w| w.iter().zip(&self.0).all(|(b, p)| p.is_none_or(|p| p == *b)))
    }

    /// Every match offset in `hay`.
    pub fn find_all<'a>(&'a self, hay: &'a [u8]) -> impl Iterator<Item = usize> + 'a {
        // Anchor on the first non-wildcard byte with memchr-style search; patterns rarely start with `?`.
        let anchor = self.0.iter().position(|b| b.is_some()).unwrap_or(0);
        let first = self.0[anchor];
        let last_start = hay.len().saturating_sub(self.0.len());
        let mut i = 0usize;
        std::iter::from_fn(move || {
            while i <= last_start && !hay.is_empty() {
                let at = i;
                i += 1;
                if let Some(f) = first
                    && hay[at + anchor] != f
                {
                    continue;
                }
                if self.matches_at(hay, at) {
                    return Some(at);
                }
            }
            None
        })
    }
}

// ---------------------------------------------------------------------------------------------------------
// Image

#[derive(Debug, Clone, Copy)]
pub struct Section {
    pub name: [u8; 8],
    pub rva: usize,
    pub size: usize,
    pub characteristics: u32,
}

impl Section {
    const EXECUTE: u32 = 0x2000_0000;

    pub fn is_code(&self) -> bool {
        self.characteristics & Self::EXECUTE != 0
    }

    pub fn range(&self) -> std::ops::Range<usize> {
        self.rva..self.rva + self.size
    }
}

/// One `.pdata` entry: a function (or a chained fragment of one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeFunction {
    pub begin: usize,
    pub end: usize,
    pub unwind: usize,
}

/// A mapped PE image addressed by RVA.
pub struct Image<'a> {
    pub bytes: &'a [u8],
    /// Address that absolute pointers stored in the image are relative to: the live module base at
    /// runtime, or the header's preferred `ImageBase` for an unrelocated file mapping.
    pub va_base: u64,
    pub timestamp: u32,
    pub sections: Vec<Section>,
    /// Sorted by `begin`.
    pub functions: Vec<RuntimeFunction>,
}

fn rd_u16(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?))
}
fn rd_u32(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}
fn rd_u64(b: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(o..o + 8)?.try_into().ok()?))
}

struct Headers {
    timestamp: u32,
    image_base: u64,
    size_of_image: usize,
    pdata: (usize, usize),
    sections: Vec<(Section, usize, usize)>, // section, raw offset, raw size
}

fn parse_headers(b: &[u8]) -> Result<Headers> {
    let bad = || ScanError("not a PE32+ image".into());
    if b.get(0..2) != Some(b"MZ") {
        return Err(bad());
    }
    let pe = rd_u32(b, 0x3C).ok_or_else(bad)? as usize;
    if b.get(pe..pe + 4) != Some(b"PE\0\0") {
        return Err(bad());
    }
    let n = rd_u16(b, pe + 6).ok_or_else(bad)? as usize;
    let timestamp = rd_u32(b, pe + 8).ok_or_else(bad)?;
    let opt_size = rd_u16(b, pe + 20).ok_or_else(bad)? as usize;
    let opt = pe + 24;
    if rd_u16(b, opt) != Some(0x20B) {
        return Err(bad());
    }
    let image_base = rd_u64(b, opt + 24).ok_or_else(bad)?;
    let size_of_image = rd_u32(b, opt + 56).ok_or_else(bad)? as usize;
    // Data directory 3 = exception table (.pdata).
    let dd = opt + 112;
    let pdata = (rd_u32(b, dd + 3 * 8).ok_or_else(bad)? as usize, rd_u32(b, dd + 3 * 8 + 4).ok_or_else(bad)? as usize);
    let mut sections = Vec::with_capacity(n);
    for i in 0..n {
        let s = opt + opt_size + 40 * i;
        let name: [u8; 8] = b.get(s..s + 8).ok_or_else(bad)?.try_into().map_err(|_| bad())?;
        let vsize = rd_u32(b, s + 8).ok_or_else(bad)? as usize;
        let rva = rd_u32(b, s + 12).ok_or_else(bad)? as usize;
        let raw_size = rd_u32(b, s + 16).ok_or_else(bad)? as usize;
        let raw = rd_u32(b, s + 20).ok_or_else(bad)? as usize;
        let characteristics = rd_u32(b, s + 36).ok_or_else(bad)?;
        sections.push((Section { name, rva, size: vsize.max(raw_size), characteristics }, raw, raw_size));
    }
    Ok(Headers { timestamp, image_base, size_of_image, pdata, sections })
}

/// Lays out a PE file's sections at their RVAs, like the loader (no relocations or imports). For tests
/// and offline tools that read the exe from disk.
pub fn map_file(file: &[u8]) -> Result<Vec<u8>> {
    let h = parse_headers(file)?;
    let mut img = vec![0u8; h.size_of_image];
    let hdr = 0x1000.min(file.len()).min(img.len());
    img[..hdr].copy_from_slice(&file[..hdr]);
    for (s, raw, raw_size) in &h.sections {
        let len = (*raw_size).min(s.size);
        let src = file.get(*raw..raw + len).ok_or(ScanError("section past end of file".into()))?;
        img.get_mut(s.rva..s.rva + len).ok_or(ScanError("section past SizeOfImage".into()))?.copy_from_slice(src);
    }
    Ok(img)
}

impl<'a> Image<'a> {
    /// `bytes` must be the whole image as mapped; `va_base` is `None` to use the header's `ImageBase`
    /// (an unrelocated file mapping).
    pub fn new(bytes: &'a [u8], va_base: Option<u64>) -> Result<Self> {
        let h = parse_headers(bytes)?;
        if h.size_of_image > bytes.len() {
            return err("image shorter than SizeOfImage");
        }
        let sections: Vec<_> = h.sections.into_iter().map(|(s, _, _)| s).collect();
        let (prva, psize) = h.pdata;
        let mut functions = Vec::with_capacity(psize / 12);
        for o in (prva..prva + psize).step_by(12) {
            let (Some(begin), Some(end), Some(unwind)) = (rd_u32(bytes, o), rd_u32(bytes, o + 4), rd_u32(bytes, o + 8))
            else {
                break;
            };
            if begin < end {
                functions.push(RuntimeFunction { begin: begin as usize, end: end as usize, unwind: unwind as usize });
            }
        }
        functions.sort_by_key(|f| f.begin);
        Ok(Self { bytes, va_base: va_base.unwrap_or(h.image_base), timestamp: h.timestamp, sections, functions })
    }

    pub fn code_sections(&self) -> impl Iterator<Item = &Section> {
        self.sections.iter().filter(|s| s.is_code())
    }

    pub fn is_code(&self, rva: usize) -> bool {
        self.code_sections().any(|s| s.range().contains(&rva))
    }

    pub fn u32_at(&self, rva: usize) -> Result<u32> {
        rd_u32(self.bytes, rva).ok_or(ScanError(format!("read past image end at {rva:#x}")))
    }

    pub fn u64_at(&self, rva: usize) -> Result<u64> {
        rd_u64(self.bytes, rva).ok_or(ScanError(format!("read past image end at {rva:#x}")))
    }

    /// RVA of an absolute pointer stored in the image (vtable slots, RTTI pointers).
    pub fn rva_of_ptr(&self, ptr: u64) -> Result<usize> {
        let rva = ptr.checked_sub(self.va_base).ok_or(ScanError(format!("pointer {ptr:#x} below image")))? as usize;
        if rva >= self.bytes.len() {
            return err(format!("pointer {ptr:#x} past image end"));
        }
        Ok(rva)
    }

    /// Only match of `pat` in code sections. Zero or several matches is an error: a non-unique pattern
    /// names the wrong address after a game update.
    pub fn find_unique_code(&self, what: &str, pat: &Pattern) -> Result<usize> {
        let mut hits = self
            .code_sections()
            .flat_map(|s| pat.find_all(&self.bytes[s.range()]).map(move |o| s.rva + o));
        let first = hits.next().ok_or(ScanError(format!("{what}: pattern not found")))?;
        if let Some(second) = hits.next() {
            return err(format!("{what}: pattern not unique ({first:#x}, {second:#x}, ...)"));
        }
        Ok(first)
    }

    /// RVA of the only NUL-terminated copy of `s` anywhere in the image.
    pub fn find_string(&self, s: &str) -> Result<usize> {
        let mut bytes = s.as_bytes().to_vec();
        bytes.push(0);
        let pat = Pattern::exact(&bytes);
        let mut hits = pat.find_all(self.bytes);
        let first = hits.next().ok_or(ScanError(format!("string {s:?} not found")))?;
        if hits.next().is_some() {
            return err(format!("string {s:?} not unique"));
        }
        Ok(first)
    }

    /// RVA of the instruction *end* (the byte after the rel32) for every RIP-relative rel32 in code that
    /// points at `target`. Byte-wise: a hit could in theory be inside another instruction, so callers walk
    /// forward from it with [`Image::find_insn`], which decodes real instruction boundaries.
    pub fn refs_to(&self, target: usize) -> Vec<usize> {
        let mut out = Vec::new();
        for s in self.code_sections() {
            let end = (s.rva + s.size).min(self.bytes.len()).saturating_sub(4);
            for i in s.rva..end {
                let disp = i32::from_le_bytes(self.bytes[i..i + 4].try_into().unwrap()) as i64;
                // disp 0 would put the target right after the instruction: that is zero padding, not a reference.
                if disp != 0 && (i as i64 + 4 + disp) as usize == target {
                    out.push(i + 4);
                }
            }
        }
        out
    }

    /// Code references to the unique string `s` (see [`Image::refs_to`]).
    pub fn string_refs(&self, s: &str) -> Result<Vec<usize>> {
        let at = self.find_string(s)?;
        let refs = self.refs_to(at);
        if refs.is_empty() {
            return err(format!("string {s:?}: no code reference"));
        }
        Ok(refs)
    }

    /// Decodes up to `max` instructions from `start` and returns the RVA of the first one whose bytes start
    /// with `pat` (AutomataMP `scan_disasm`). Like AutomataMP, it does not stop at `ret`; `max` bounds the walk.
    pub fn find_insn(&self, start: usize, max: usize, pat: &Pattern) -> Result<usize> {
        self.walk(start, max, |rva, _| pat.matches_at(self.bytes, rva))
            .ok_or(ScanError(format!("no {pat:02x?} within {max} instructions of {start:#x}")))
    }

    /// Decodes up to `max` instructions from `start`, returning the RVA of the first where `f` is true.
    pub fn walk(&self, start: usize, max: usize, mut f: impl FnMut(usize, &Instruction) -> bool) -> Option<usize> {
        let code = self.bytes.get(start..)?;
        let mut dec = Decoder::with_ip(64, code, start as u64, DecoderOptions::NONE);
        let mut ins = Instruction::default();
        for _ in 0..max {
            if !dec.can_decode() {
                return None;
            }
            dec.decode_out(&mut ins);
            if ins.is_invalid() {
                return None;
            }
            let rva = ins.ip() as usize;
            if f(rva, &ins) {
                return Some(rva);
            }
        }
        None
    }

    /// Target RVA of the RIP-relative memory operand or the near branch of the instruction at `at`.
    pub fn insn_target(&self, at: usize) -> Result<usize> {
        let code = self.bytes.get(at..).ok_or(ScanError(format!("{at:#x} past image end")))?;
        let mut dec = Decoder::with_ip(64, code, at as u64, DecoderOptions::NONE);
        let ins = dec.decode();
        let t = if ins.is_ip_rel_memory_operand() {
            ins.ip_rel_memory_address()
        } else if (0..ins.op_count()).any(|i| matches!(ins.op_kind(i), OpKind::NearBranch64)) {
            ins.near_branch64()
        } else {
            return err(format!("instruction at {at:#x} has no RIP-relative target"));
        };
        if t as usize >= self.bytes.len() {
            return err(format!("target of {at:#x} past image end"));
        }
        Ok(t as usize)
    }

    /// The `.pdata` function containing `rva`, following chained-unwind fragments back to the primary
    /// entry, so the result is the real function start.
    pub fn function_at(&self, rva: usize) -> Result<RuntimeFunction> {
        let i = self.functions.partition_point(|f| f.begin <= rva);
        let mut f = *i
            .checked_sub(1)
            .and_then(|i| self.functions.get(i))
            .filter(|f| rva < f.end)
            .ok_or(ScanError(format!("{rva:#x} is not inside a known function")))?;
        // UNWIND_INFO: flags in the top 5 bits of byte 0; UNW_FLAG_CHAININFO = 4. The parent entry follows
        // the unwind code array (count at byte 2, padded to an even count).
        for _ in 0..32 {
            let b0 = *self.bytes.get(f.unwind).ok_or(ScanError("unwind info past image end".into()))?;
            if (b0 >> 3) & 4 == 0 {
                return Ok(f);
            }
            let count = *self.bytes.get(f.unwind + 2).ok_or(ScanError("unwind info past image end".into()))? as usize;
            let parent = f.unwind + 4 + count.div_ceil(2) * 2 * 2;
            f = RuntimeFunction {
                begin: self.u32_at(parent)? as usize,
                end: self.u32_at(parent + 4)? as usize,
                unwind: self.u32_at(parent + 8)? as usize,
            };
        }
        err("unwind chain too long")
    }

    /// Checks that patching `len` bytes at `site` is safe: `site` and `site + len` lie in one function,
    /// `site` is an instruction boundary, and no direct branch in that function lands inside the patched
    /// bytes (it would jump into the middle of our jmp). Indirect jumps (switch tables) cannot be checked.
    pub fn check_hook_site(&self, site: usize, len: usize) -> Result<()> {
        let f = self.function_at(site)?;
        let code = self.bytes.get(f.begin..f.end).ok_or(ScanError("function past image end".into()))?;
        let decode = || {
            let mut dec = Decoder::with_ip(64, code, f.begin as u64, DecoderOptions::NONE);
            std::iter::from_fn(move || dec.can_decode().then(|| dec.decode()))
        };
        // Pass 1: the patch moves every instruction that starts inside [site, site + len).
        let mut on_boundary = false;
        let mut covered_end = site + len;
        for ins in decode() {
            let ip = ins.ip() as usize;
            on_boundary |= ip == site;
            if ip >= site && ip < site + len {
                covered_end = covered_end.max(ins.next_ip() as usize);
            }
        }
        if !on_boundary {
            return err(format!("{site:#x} is not an instruction boundary"));
        }
        if covered_end > f.end {
            return err(format!("patch at {site:#x} runs past the end of its function"));
        }
        // Pass 2: no direct branch may land on a moved instruction other than the first.
        for ins in decode() {
            let ip = ins.ip() as usize;
            let target = match ins.flow_control() {
                FlowControl::UnconditionalBranch | FlowControl::ConditionalBranch | FlowControl::Call
                    if (0..ins.op_count()).any(|i| matches!(ins.op_kind(i), OpKind::NearBranch64)) =>
                {
                    Some(ins.near_branch64() as usize)
                }
                _ => None,
            };
            if let Some(t) = target
                && t > site
                && t < covered_end
            {
                return err(format!("branch at {ip:#x} jumps into the patched bytes at {t:#x}"));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pattern_parse_and_find() {
        assert!(Pattern::parse("").is_err());
        assert!(Pattern::parse("48 zz").is_err());
        let p = Pattern::parse("48 8B ?? 10").unwrap();
        let hay = [0x90, 0x48, 0x8B, 0x05, 0x10, 0x48, 0x8B, 0xFF, 0x10];
        assert_eq!(p.find_all(&hay).collect::<Vec<_>>(), vec![1, 5]);
        let w = Pattern::parse("? 8B").unwrap();
        assert_eq!(w.find_all(&hay).collect::<Vec<_>>(), vec![1, 5]);
        assert_eq!(Pattern::exact(b"ab").find_all(b"xab").next(), Some(1));
        assert_eq!(Pattern::exact(b"abc").find_all(b"ab").next(), None);
    }

    /// A minimal PE32+ image with one code section, built by hand.
    fn fake_image(code: &[u8]) -> Vec<u8> {
        let mut b = vec![0u8; 0x2000];
        b[0..2].copy_from_slice(b"MZ");
        b[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        b[0x80..0x84].copy_from_slice(b"PE\0\0");
        b[0x86..0x88].copy_from_slice(&1u16.to_le_bytes()); // sections
        b[0x94..0x96].copy_from_slice(&0xF0u16.to_le_bytes()); // optional header size
        let opt = 0x98;
        b[opt..opt + 2].copy_from_slice(&0x20Bu16.to_le_bytes());
        b[opt + 24..opt + 32].copy_from_slice(&0x1_4000_0000u64.to_le_bytes());
        b[opt + 56..opt + 60].copy_from_slice(&0x2000u32.to_le_bytes());
        let s = opt + 0xF0;
        b[s..s + 5].copy_from_slice(b".text");
        b[s + 8..s + 12].copy_from_slice(&0x1000u32.to_le_bytes());
        b[s + 12..s + 16].copy_from_slice(&0x1000u32.to_le_bytes());
        b[s + 36..s + 40].copy_from_slice(&0x6000_0020u32.to_le_bytes());
        b[0x1000..0x1000 + code.len()].copy_from_slice(code);
        b
    }

    #[test]
    fn string_reference_and_insn_walk() {
        // 0x1000: lea rcx,[rip+X] -> "Hi\0" at 0x1100; 0x1007: mov rax,[rip+Y]; ret
        let mut code = vec![0x48, 0x8D, 0x0D, 0, 0, 0, 0, 0x48, 0x8B, 0x05, 0, 0, 0, 0, 0xC3];
        code[3..7].copy_from_slice(&(0x1100i32 - 0x1007).to_le_bytes());
        code[10..14].copy_from_slice(&(0x1200i32 - 0x100E).to_le_bytes());
        let mut bytes = fake_image(&code);
        bytes[0x1100..0x1103].copy_from_slice(b"Hi\0");
        let img = Image::new(&bytes, None).unwrap();
        assert_eq!(img.va_base, 0x1_4000_0000);
        assert_eq!(img.find_string("Hi").unwrap(), 0x1100);
        assert_eq!(img.string_refs("Hi").unwrap(), vec![0x1007]);
        let mov = img.find_insn(0x1007, 5, &Pattern::parse("48 8B 05").unwrap()).unwrap();
        assert_eq!(mov, 0x1007);
        assert_eq!(img.insn_target(mov).unwrap(), 0x1200);
        assert_eq!(img.insn_target(0x1000).unwrap(), 0x1100);
        assert!(img.find_insn(0x1000, 5, &Pattern::parse("E8").unwrap()).is_err()); // not within 5 instructions
        assert!(img.find_unique_code("lea", &Pattern::parse("48 8D 0D").unwrap()).is_ok());
        assert!(img.find_unique_code("rex", &Pattern::parse("48").unwrap()).is_err()); // not unique
    }

    #[test]
    fn hook_site_check() {
        // 0x1000: jmp short +0x03 (to 0x1005, inside a 14-byte patch at 0x1002)... built as:
        // 0x1000 EB 03          jmp 0x1005
        // 0x1002 90 90 90       nops
        // 0x1005 90 x 20        nops
        // 0x1019 C3             ret
        let mut code = vec![0xEB, 0x03];
        code.extend([0x90; 23]);
        code.push(0xC3);
        let mut bytes = fake_image(&code);
        // .pdata in the header page is easier than a section: point data directory 3 at 0x1800.
        let dd = 0x98 + 112 + 3 * 8;
        bytes[dd..dd + 4].copy_from_slice(&0x1800u32.to_le_bytes());
        bytes[dd + 4..dd + 8].copy_from_slice(&12u32.to_le_bytes());
        bytes[0x1800..0x1804].copy_from_slice(&0x1000u32.to_le_bytes());
        bytes[0x1804..0x1808].copy_from_slice(&0x101Au32.to_le_bytes());
        bytes[0x1808..0x180C].copy_from_slice(&0x1900u32.to_le_bytes());
        bytes[0x1900] = 1; // version 1, no flags
        let img = Image::new(&bytes, None).unwrap();
        assert_eq!(img.function_at(0x1010).unwrap().begin, 0x1000);
        assert!(img.function_at(0x2000).is_err());
        assert!(img.check_hook_site(0x1002, 14).is_err()); // jmp lands inside
        assert!(img.check_hook_site(0x1005, 14).is_ok()); // jmp lands on the first patched byte: fine
        assert!(img.check_hook_site(0x1001, 4).is_err()); // not a boundary
        assert!(img.check_hook_site(0x1010, 14).is_err()); // runs past the function end
    }
}
