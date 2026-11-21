//! Reading the dynamic section of a little-endian ELF64 file, as the loader
//! sees it. `object` supplies the layouts; which segments are trusted is
//! decided here.

use object::LittleEndian;
use object::elf::{
    DT_NULL, DT_RPATH, DT_RUNPATH, DT_SONAME, DT_STRTAB, DynamicTag, ELFMAG, FileHeader64,
    PT_DYNAMIC, PT_LOAD, ProgramHeader64,
};
use object::read::elf::{Dyn, FileHeader, ProgramHeader};

type Header = FileHeader64<LittleEndian>;
type Segment = ProgramHeader64<LittleEndian>;

const DYNAMIC_ENTRY_SIZE: u64 = 16;

pub(crate) fn is_elf(bytes: &[u8]) -> bool {
    bytes.starts_with(&ELFMAG)
}

/// The soname of a shared object, if it has one.
pub(crate) fn soname(elf: &[u8]) -> Option<String> {
    dynamic_strings(elf, DT_SONAME)?.into_iter().next()
}

/// Every RPATH and RUNPATH entry, `None` when `elf` is not one ovfetch can read.
/// Empty segments (`""`, leading/trailing `:`, or `::`) are preserved because
/// the dynamic linker treats an empty segment as the current working directory.
pub(crate) fn search_paths(elf: &[u8]) -> Option<Vec<String>> {
    let mut paths = Vec::new();
    for tag in [DT_RPATH, DT_RUNPATH] {
        for list in dynamic_strings(elf, tag)? {
            paths.extend(list.split(':').map(str::to_owned));
        }
    }
    Some(paths)
}

/// Strings for one dynamic tag, read the way the loader reads them: the
/// `PT_DYNAMIC` segment, with `DT_STRTAB` mapped to a file offset through the
/// `PT_LOAD` segments. Section headers are not used; they can be stripped, or
/// disagree with what the loader sees. `None` when the file is not a
/// little-endian ELF64, or is malformed. A statically linked file has none.
fn dynamic_strings(file: &[u8], tag: DynamicTag) -> Option<Vec<String>> {
    let endian = LittleEndian;
    let header = Header::parse(file).ok()?;
    header.endian().ok()?;
    let segments = header.program_headers(endian, file).ok()?;
    let Some(dynamic) = segments.iter().find(|s| s.p_type(endian) == PT_DYNAMIC) else {
        return Some(Vec::new());
    };
    if dynamic.p_filesz(endian) % DYNAMIC_ENTRY_SIZE != 0 {
        return None;
    }
    let (mut string_table, mut string_offsets) = (None, Vec::new());
    for entry in dynamic.dynamic(endian, file).ok()?? {
        let entry_tag = entry.d_tag(endian);
        if entry_tag == DT_NULL {
            break;
        } else if entry_tag == DT_STRTAB {
            string_table = Some(entry.d_val(endian));
        } else if entry_tag == tag {
            string_offsets.push(entry.d_val(endian));
        }
    }
    if string_offsets.is_empty() {
        return Some(Vec::new());
    }
    let table = file_offset(segments, string_table?)?;
    string_offsets
        .into_iter()
        .map(|offset| {
            let start = usize::try_from(table.checked_add(offset)?).ok()?;
            let name = file.get(start..)?;
            let end = name.iter().position(|&c| c == 0)?;
            // Lossy, so one odd byte can't hide the rest of the path.
            Some(String::from_utf8_lossy(&name[..end]).into_owned())
        })
        .collect()
}

/// Where the loader would find `address` in the file: inside the file-backed
/// part of a `PT_LOAD` segment.
fn file_offset(segments: &[Segment], address: u64) -> Option<u64> {
    let endian = LittleEndian;
    segments
        .iter()
        .find(|s| {
            let start = s.p_vaddr(endian);
            s.p_type(endian) == PT_LOAD && start <= address && address - start < s.p_filesz(endian)
        })
        .and_then(|s| (address - s.p_vaddr(endian)).checked_add(s.p_offset(endian)))
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

    const BASE: u64 = 0x40_0000;
    const PROGRAM_HEADERS: u64 = 64;
    const PROGRAM_HEADER_SIZE: u64 = 56;
    const LOAD_HEADER: u64 = PROGRAM_HEADERS;
    const DYNAMIC_HEADER: u64 = PROGRAM_HEADERS + PROGRAM_HEADER_SIZE;
    const DYNAMIC: u64 = PROGRAM_HEADERS + 2 * PROGRAM_HEADER_SIZE;
    /// Offsets inside a program header.
    const P_TYPE: u64 = 0;
    const P_OFFSET: u64 = 0x08;
    const P_VADDR: u64 = 0x10;
    const P_FILESZ: u64 = 0x20;
    /// Offsets inside the file header.
    const E_PHOFF: u64 = 0x20;

    const STRTAB: u64 = 5;
    const SONAME: u64 = 14;
    const RPATH: u64 = 15;
    const RUNPATH: u64 = 29;
    const NULL: u64 = 0;

    fn put(file: &mut [u8], at: u64, value: u64, len: usize) {
        file[at as usize..at as usize + len].copy_from_slice(&value.to_le_bytes()[..len])
    }

    /// The address the string table gets in a file with `entries` dynamic entries.
    fn strtab_address(entries: usize) -> u64 {
        BASE + DYNAMIC + entries as u64 * 16
    }

    /// A minimal ELF64 shared object with no section headers, so only the
    /// program headers say where its dynamic entries and strings are: one
    /// PT_LOAD over the whole file, one PT_DYNAMIC over `entries`, then
    /// `strings`.
    fn elf_with_dynamic(entries: &[(u64, u64)], strings: &[u8]) -> Vec<u8> {
        let dynamic_size = entries.len() as u64 * 16;
        let strtab = DYNAMIC + dynamic_size;
        let len = strtab + strings.len() as u64;
        let mut f = vec![0u8; len as usize];
        f[..4].copy_from_slice(b"\x7fELF");
        f[4] = 2; // ELFCLASS64
        f[5] = 1; // little-endian
        f[6] = 1;
        put(&mut f, 0x10, 3, 2); // ET_DYN
        put(&mut f, 0x12, 62, 2); // x86-64
        put(&mut f, E_PHOFF, PROGRAM_HEADERS, 8);
        put(&mut f, 0x36, PROGRAM_HEADER_SIZE, 2);
        put(&mut f, 0x38, 2, 2);
        for (at, kind, offset, size) in [
            (LOAD_HEADER, 1u64, 0, len),
            (DYNAMIC_HEADER, 2, DYNAMIC, dynamic_size),
        ] {
            put(&mut f, at + P_TYPE, kind, 4);
            put(&mut f, at + P_OFFSET, offset, 8);
            put(&mut f, at + P_VADDR, BASE + offset, 8);
            put(&mut f, at + P_FILESZ, size, 8);
        }
        for (i, (tag, value)) in entries.iter().enumerate() {
            put(&mut f, DYNAMIC + i as u64 * 16, *tag, 8);
            put(&mut f, DYNAMIC + i as u64 * 16 + 8, *value, 8);
        }
        f[strtab as usize..].copy_from_slice(strings);
        f
    }

    pub(crate) fn elf_with_runpath(runpath: &[u8]) -> Vec<u8> {
        let strings = [b"\0".as_slice(), runpath, b"\0"].concat();
        elf_with_dynamic(
            &[(STRTAB, strtab_address(3)), (RUNPATH, 1), (NULL, 0)],
            &strings,
        )
    }

    fn paths(elf: &[u8]) -> Option<Vec<String>> {
        search_paths(elf)
    }

    #[test]
    fn rpath_and_runpath_are_both_read_and_split() {
        let elf = elf_with_dynamic(
            &[
                (RPATH, 1),
                (STRTAB, strtab_address(5)),
                (RUNPATH, 9),
                (SONAME, 21),
                (NULL, 0),
            ],
            b"\0/opt/a:\0$ORIGIN::/b\0libx.so.1\0",
        );
        assert_eq!(
            paths(&elf),
            Some(vec![
                "/opt/a".into(),
                "".into(),
                "$ORIGIN".into(),
                "".into(),
                "/b".into()
            ])
        );
        assert_eq!(soname(&elf).as_deref(), Some("libx.so.1"));
    }

    #[test]
    fn a_library_with_no_search_path_has_an_empty_one() {
        let no_tag = elf_with_dynamic(&[(STRTAB, strtab_address(2)), (NULL, 0)], b"\0");
        assert_eq!(paths(&no_tag), Some(vec![]));
        assert_eq!(soname(&no_tag), None);

        let mut statically_linked = elf_with_runpath(b"/home/u/build");
        put(&mut statically_linked, DYNAMIC_HEADER + P_TYPE, 4, 4);
        assert_eq!(paths(&statically_linked), Some(vec![]));
    }

    #[test]
    fn entries_after_the_terminator_are_not_read() {
        let elf = elf_with_dynamic(
            &[(STRTAB, strtab_address(3)), (NULL, 0), (RUNPATH, 1)],
            b"\0/home/u/build\0",
        );
        assert_eq!(paths(&elf), Some(vec![]));
    }

    #[test]
    fn a_file_that_is_not_little_endian_elf64_is_unreadable() {
        let good = elf_with_runpath(b"$ORIGIN");
        assert!(paths(&good).is_some());
        for (at, value) in [(0usize, b'X'), (4, 1), (5, 2)] {
            let mut bad = good.clone();
            bad[at] = value;
            assert_eq!(paths(&bad), None, "byte {at} = {value}");
            assert_eq!(soname(&bad), None);
        }
        assert_eq!(paths(b""), None);
        assert_eq!(paths(b"\x7fELF"), None);
        assert_eq!(paths(b"not elf at all, just text"), None);
    }

    #[test]
    fn every_truncation_of_a_library_is_unreadable() {
        let whole = elf_with_runpath(b"$ORIGIN:/home/u/build/lib");
        assert!(paths(&whole).is_some());
        for len in 0..whole.len() {
            assert_eq!(paths(&whole[..len]), None, "cut at {len}");
        }
    }

    #[test]
    fn a_search_path_that_cannot_be_located_is_unreadable() {
        let strings = b"\0/home/u/build\0";
        let no_string_table = elf_with_dynamic(&[(RUNPATH, 1), (NULL, 0)], strings);
        assert_eq!(paths(&no_string_table), None);

        let unmapped_string_table =
            elf_with_dynamic(&[(STRTAB, 0x10), (RUNPATH, 1), (NULL, 0)], strings);
        assert_eq!(paths(&unmapped_string_table), None);

        let string_past_the_end = elf_with_dynamic(
            &[(STRTAB, strtab_address(3)), (RUNPATH, 10_000), (NULL, 0)],
            strings,
        );
        assert_eq!(paths(&string_past_the_end), None);

        let unterminated_string = elf_with_dynamic(
            &[(STRTAB, strtab_address(3)), (RUNPATH, 1), (NULL, 0)],
            b"\0/home/u/build",
        );
        assert_eq!(paths(&unterminated_string), None);
    }

    #[test]
    fn offsets_that_overflow_or_leave_the_file_are_unreadable() {
        let good = elf_with_runpath(b"/home/u/build");
        for (at, value) in [
            (E_PHOFF, u64::MAX),
            (E_PHOFF, u64::MAX - 100),
            (E_PHOFF, good.len() as u64),
            (DYNAMIC_HEADER + P_OFFSET, u64::MAX - 8),
            (DYNAMIC_HEADER + P_OFFSET, good.len() as u64),
            (LOAD_HEADER + P_VADDR, u64::MAX),
            (LOAD_HEADER + P_FILESZ, 0),
            (LOAD_HEADER + P_OFFSET, u64::MAX),
        ] {
            let mut bad = good.clone();
            put(&mut bad, at, value, 8);
            assert_eq!(paths(&bad), None, "{at:#x} = {value:#x}");
        }
    }

    #[test]
    fn a_dynamic_segment_the_file_cannot_hold_is_unreadable() {
        let good = elf_with_runpath(b"/home/u/build");
        for size in [3 * 16 + 8, 1 << 40] {
            let mut bad = good.clone();
            put(&mut bad, DYNAMIC_HEADER + P_FILESZ, size, 8);
            assert_eq!(paths(&bad), None, "p_filesz = {size}");
        }
        let mut odd_header_size = good.clone();
        put(&mut odd_header_size, 0x36, 64, 2);
        assert_eq!(paths(&odd_header_size), None);
    }
}
