//! MSVC x64 RTTI walks over the game [`Image`]. Port of AutomataMP `shared/utility/RTTI.cpp`
//! (`derives_from`), plus a static vtable lookup by class name.
//!
//! Layout (all "RVA" fields are 32-bit offsets from the module base):
//! - `vtable[-1]` -> CompleteObjectLocator: signature u32 (=1), offset u32, cd_offset u32,
//!   type_descriptor RVA (+0x0C), class_hierarchy RVA (+0x10), self RVA (+0x14).
//! - ClassHierarchyDescriptor: num_base_classes u32 (+0x08), base_class_array RVA (+0x0C).
//! - BaseClassArray: u32 RVAs of BaseClassDescriptors, whose first field is a type_descriptor RVA.
//! - TypeDescriptor: vtable u64, spare u64, decorated name at +0x10 (".?AVPl0000@@").

use crate::scan::{Image, Result, ScanError, err};

/// Decorated name of a class, as stored in its TypeDescriptor.
pub fn decorated(class: &str) -> String {
    format!(".?AV{class}@@")
}

fn type_name(img: &Image, td: usize) -> Result<String> {
    let s = img.bytes.get(td + 0x10..).ok_or(ScanError("type descriptor past image end".into()))?;
    let n = s.iter().take(256).position(|&b| b == 0).ok_or(ScanError("type name not terminated".into()))?;
    Ok(String::from_utf8_lossy(&s[..n]).into_owned())
}

/// Decorated names of the class and all its bases, for the object whose vtable is at `vtable` (RVA).
pub fn class_hierarchy(img: &Image, vtable: usize) -> Result<Vec<String>> {
    let col_ptr = img.u64_at(vtable.checked_sub(8).ok_or(ScanError("vtable at image start".into()))?)?;
    let col = img.rva_of_ptr(col_ptr)?;
    if img.u32_at(col)? != 1 || img.u32_at(col + 0x14)? as usize != col {
        return err(format!("no complete object locator for vtable {vtable:#x}"));
    }
    let chd = img.u32_at(col + 0x10)? as usize;
    let n = img.u32_at(chd + 8)? as usize;
    if n > 64 {
        return err("implausible base class count");
    }
    let bca = img.u32_at(chd + 0xC)? as usize;
    (0..n)
        .map(|i| {
            let bcd = img.u32_at(bca + 4 * i)? as usize;
            type_name(img, img.u32_at(bcd)? as usize)
        })
        .collect()
}

/// RVA of the primary vtable (object offset 0) of class `class` (undecorated, e.g. "Pl0000").
pub fn find_vtable(img: &Image, class: &str) -> Result<usize> {
    let name = decorated(class);
    let td = img.find_string(&name)? - 0x10;
    // Complete object locators live in read-only data; find the one for offset 0 that points at `td`.
    let mut cols = Vec::new();
    for s in img.sections.iter().filter(|s| !s.is_code()) {
        let end = (s.rva + s.size).min(img.bytes.len()).saturating_sub(0x18);
        for o in (s.rva..end).step_by(4) {
            if img.u32_at(o + 0xC)? as usize == td
                && img.u32_at(o)? == 1
                && img.u32_at(o + 4)? == 0
                && img.u32_at(o + 0x14)? as usize == o
            {
                cols.push(o);
            }
        }
    }
    let [col] = cols[..] else {
        return err(format!("{class}: expected one object locator at offset 0, found {}", cols.len()));
    };
    let col_va = (img.va_base + col as u64).to_le_bytes();
    let mut metas = img
        .sections
        .iter()
        .filter(|s| !s.is_code())
        .flat_map(|s| (s.rva..(s.rva + s.size).min(img.bytes.len()).saturating_sub(8)).step_by(8))
        .filter(|&o| img.bytes[o..o + 8] == col_va);
    let meta = metas.next().ok_or(ScanError(format!("{class}: vtable not found")))?;
    if metas.next().is_some() {
        return err(format!("{class}: several vtables reference the locator"));
    }
    Ok(meta + 8)
}

/// Every primary vtable in the image with its class chain (most-derived first). Slow: a full scan of the
/// data sections, for offline tests.
#[cfg(test)]
pub fn all_vtables(img: &Image) -> Vec<(usize, Vec<String>)> {
    let mut out = Vec::new();
    for s in img.sections.iter().filter(|s| !s.is_code()) {
        let end = (s.rva + s.size).min(img.bytes.len()).saturating_sub(16);
        for at in (s.rva..end).step_by(8) {
            let Ok(col) = img.u64_at(at).and_then(|p| img.rva_of_ptr(p)) else { continue };
            let is_primary_col = img.u32_at(col).ok() == Some(1)
                && img.u32_at(col + 4).ok() == Some(0)
                && img.u32_at(col + 0x14).ok().map(|x| x as usize) == Some(col);
            if is_primary_col && let Ok(chain) = class_hierarchy(img, at + 8) {
                out.push((at + 8, chain));
            }
        }
    }
    out
}

/// RVA of the function in vtable slot `index`.
pub fn vtable_slot(img: &Image, vtable: usize, index: usize) -> Result<usize> {
    let f = img.rva_of_ptr(img.u64_at(vtable + 8 * index)?)?;
    if !img.is_code(f) {
        return err(format!("vtable slot {index} does not point at code"));
    }
    Ok(f)
}
