//! Reading the dynamic section of a little-endian ELF64 file.

const MAGIC: &[u8] = b"\x7fELF";
const CLASS_64: u8 = 2;
const LITTLE_ENDIAN: u8 = 1;

const PT_LOAD: usize = 1;
const PT_DYNAMIC: usize = 2;
const DYNAMIC_ENTRY_SIZE: usize = 16;

const DT_NULL: usize = 0;
const DT_STRTAB: usize = 5;
const DT_SONAME: usize = 14;
const DT_RPATH: usize = 15;
const DT_RUNPATH: usize = 29;

pub(crate) fn is_elf(bytes: &[u8]) -> bool {
    bytes.starts_with(MAGIC)
}

/// The soname of a shared object, if it has one.
pub(crate) fn soname(elf: &[u8]) -> Option<String> {
    dynamic_strings(elf, DT_SONAME)?.into_iter().next()
}

/// Every RPATH and RUNPATH entry, `None` when `elf` is not one ovfetch can read.
pub(crate) fn search_paths(elf: &[u8]) -> Option<Vec<String>> {
    let mut paths = Vec::new();
    for tag in [DT_RPATH, DT_RUNPATH] {
        for list in dynamic_strings(elf, tag)? {
            paths.extend(list.split(':').filter(|p| !p.is_empty()).map(str::to_owned));
        }
    }
    Some(paths)
}

struct ProgramHeader {
    kind: usize,
    file_offset: usize,
    virtual_address: usize,
    file_size: usize,
}

fn little_endian_uint(bytes: &[u8], at: usize, len: usize) -> Option<usize> {
    let raw = bytes.get(at..at.checked_add(len)?)?;
    Some(raw.iter().rev().fold(0usize, |a, &x| (a << 8) | x as usize))
}

fn program_headers(elf: &[u8]) -> Option<Vec<ProgramHeader>> {
    let field =
        |at: usize, off: usize, len: usize| little_endian_uint(elf, at.checked_add(off)?, len);
    if elf.get(..4)? != MAGIC || *elf.get(4)? != CLASS_64 || *elf.get(5)? != LITTLE_ENDIAN {
        return None;
    }
    let (table, entry_size, count) = (field(0, 0x20, 8)?, field(0, 0x36, 2)?, field(0, 0x38, 2)?);
    (0..count)
        .map(|i| {
            let at = table.checked_add(i.checked_mul(entry_size)?)?;
            Some(ProgramHeader {
                kind: field(at, 0, 4)?,
                file_offset: field(at, 0x08, 8)?,
                virtual_address: field(at, 0x10, 8)?,
                file_size: field(at, 0x20, 8)?,
            })
        })
        .collect()
}

/// Strings for one dynamic tag, read the way the loader reads them: the
/// `PT_DYNAMIC` segment, with `DT_STRTAB` mapped to a file offset through the
/// `PT_LOAD` segments. Section headers are not used; they can be stripped, or
/// disagree with what the loader sees. `None` when the file is not a
/// little-endian ELF64, or is malformed. A statically linked file has none.
fn dynamic_strings(elf: &[u8], tag: usize) -> Option<Vec<String>> {
    let headers = program_headers(elf)?;
    let Some(dynamic) = headers.iter().find(|h| h.kind == PT_DYNAMIC) else {
        return Some(Vec::new());
    };
    let file_offset_of = |address: usize| {
        headers
            .iter()
            .find(|h| {
                h.kind == PT_LOAD
                    && h.virtual_address <= address
                    && address - h.virtual_address < h.file_size
            })
            .and_then(|h| (address - h.virtual_address).checked_add(h.file_offset))
    };
    let entry = |at: usize, off: usize| little_endian_uint(elf, at.checked_add(off)?, 8);
    let (mut string_table, mut string_offsets) = (None, Vec::new());
    let end = dynamic.file_offset.checked_add(dynamic.file_size)?;
    for at in (dynamic.file_offset..end).step_by(DYNAMIC_ENTRY_SIZE) {
        match entry(at, 0)? {
            DT_NULL => break,
            DT_STRTAB => string_table = Some(entry(at, 8)?),
            t if t == tag => string_offsets.push(entry(at, 8)?),
            _ => {}
        }
    }
    if string_offsets.is_empty() {
        return Some(Vec::new());
    }
    let base = file_offset_of(string_table?)?;
    string_offsets
        .into_iter()
        .map(|offset| {
            let name = elf.get(base.checked_add(offset)?..)?;
            let end = name.iter().position(|&c| c == 0)?;
            // Lossy, so one odd byte can't hide the rest of the path.
            Some(String::from_utf8_lossy(&name[..end]).into_owned())
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn reads_soname_from_a_shared_object() {
        let Some(libc) = ["/lib/x86_64-linux-gnu/libc.so.6", "/usr/lib64/libc.so.6"]
            .iter()
            .find_map(|p| fs::read(p).ok())
        else {
            return;
        };
        assert_eq!(soname(&libc).as_deref(), Some("libc.so.6"));
        assert_eq!(soname(b"not elf"), None);
    }

    /// A minimal ELF64 shared object with one DT_RUNPATH and no section
    /// headers, so only the program headers say where its dynamic strings are.
    pub(crate) fn elf_with_runpath(runpath: &[u8]) -> Vec<u8> {
        const BASE: u64 = 0x40_0000;
        let (ph, dynamic, strtab) = (64u64, 64 + 2 * 56, 64 + 2 * 56 + 3 * 16);
        let mut strings = vec![0u8];
        strings.extend_from_slice(runpath);
        strings.push(0);
        let len = strtab + strings.len() as u64;
        let mut f = vec![0u8; len as usize];
        let put = |f: &mut Vec<u8>, at: u64, v: u64, n: usize| {
            f[at as usize..at as usize + n].copy_from_slice(&v.to_le_bytes()[..n])
        };
        f[..4].copy_from_slice(b"\x7fELF");
        f[4] = 2; // ELFCLASS64
        f[5] = 1; // little-endian
        f[6] = 1;
        put(&mut f, 0x10, 3, 2); // ET_DYN
        put(&mut f, 0x12, 62, 2); // x86-64
        put(&mut f, 0x20, ph, 8);
        put(&mut f, 0x36, 56, 2);
        put(&mut f, 0x38, 2, 2);
        // PT_LOAD over the whole file, PT_DYNAMIC over the three entries.
        for (i, (ty, off, size)) in [(1u64, 0, len), (2, dynamic, 3 * 16)]
            .into_iter()
            .enumerate()
        {
            let at = ph + i as u64 * 56;
            put(&mut f, at, ty, 4);
            put(&mut f, at + 0x08, off, 8);
            put(&mut f, at + 0x10, BASE + off, 8);
            put(&mut f, at + 0x20, size, 8);
        }
        for (i, (tag, val)) in [(5u64, BASE + strtab), (29, 1), (0, 0)]
            .into_iter()
            .enumerate()
        {
            put(&mut f, dynamic + i as u64 * 16, tag, 8);
            put(&mut f, dynamic + i as u64 * 16 + 8, val, 8);
        }
        f[strtab as usize..].copy_from_slice(&strings);
        f
    }
}
