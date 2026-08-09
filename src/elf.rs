//! ELF inspection, hand-rolled for the two targets that matter (linux x86-64 and
//! aarch64, both ELF64 little-endian).
//!
//! Shelling out to `readelf` would work, but a PIE builder that cannot itself
//! read an ELF header is a strange thing, and this keeps the check identical on
//! every runner regardless of which binutils happens to be installed.

use anyhow::{bail, Context, Result};
use std::path::Path;

const ET_DYN: u16 = 3;
const PT_DYNAMIC: u32 = 2;
const PT_INTERP: u32 = 3;
const DT_NULL: u64 = 0;
const DT_NEEDED: u64 = 1;
const DT_STRTAB: u64 = 5;
const DT_FLAGS_1: u64 = 0x6fff_fffb;
const DF_1_PIE: u64 = 0x0800_0000;

#[derive(Debug)]
pub struct ElfInfo {
    pub is_dyn: bool,
    pub has_pie_flag: bool,
    pub has_interp: bool,
    pub needed: Vec<String>,
}

impl ElfInfo {
    /// A shared library is also `ET_DYN` and also carries no `PT_INTERP`; the
    /// combination of all three is what distinguishes a real PIE executable.
    pub fn is_pie_executable(&self) -> bool {
        self.is_dyn && self.has_pie_flag && self.has_interp
    }

    pub fn explain_failure(&self) -> String {
        let mut reasons = Vec::new();
        if !self.is_dyn {
            reasons.push("ELF type is not DYN (built as a fixed-address executable)");
        }
        if !self.has_pie_flag {
            reasons.push("DT_FLAGS_1 does not carry PIE");
        }
        if !self.has_interp {
            reasons.push("no PT_INTERP segment (this is a shared library, not an executable)");
        }
        reasons.join("; ")
    }
}

fn u16_at(bytes: &[u8], offset: usize) -> Result<u16> {
    let slice = bytes
        .get(offset..offset + 2)
        .context("truncated ELF: expected 2 bytes")?;
    Ok(u16::from_le_bytes([slice[0], slice[1]]))
}

fn u32_at(bytes: &[u8], offset: usize) -> Result<u32> {
    let slice = bytes
        .get(offset..offset + 4)
        .context("truncated ELF: expected 4 bytes")?;
    Ok(u32::from_le_bytes(slice.try_into().unwrap()))
}

fn u64_at(bytes: &[u8], offset: usize) -> Result<u64> {
    let slice = bytes
        .get(offset..offset + 8)
        .context("truncated ELF: expected 8 bytes")?;
    Ok(u64::from_le_bytes(slice.try_into().unwrap()))
}

pub fn inspect(path: &Path) -> Result<ElfInfo> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    parse(&bytes).with_context(|| format!("parsing {}", path.display()))
}

fn parse(bytes: &[u8]) -> Result<ElfInfo> {
    if bytes.len() < 64 || &bytes[0..4] != b"\x7fELF" {
        bail!("not an ELF file");
    }
    if bytes[4] != 2 {
        bail!("not a 64-bit ELF file (only ELF64 is supported)");
    }
    if bytes[5] != 1 {
        bail!("not a little-endian ELF file (only ELFDATA2LSB is supported)");
    }

    let e_type = u16_at(bytes, 16)?;
    let e_phoff = u64_at(bytes, 32)? as usize;
    let e_phentsize = u16_at(bytes, 54)? as usize;
    let e_phnum = u16_at(bytes, 56)? as usize;

    let mut has_interp = false;
    let mut dynamic: Option<(usize, usize)> = None;

    for i in 0..e_phnum {
        let ph = e_phoff + i * e_phentsize;
        let p_type = u32_at(bytes, ph)?;
        let p_offset = u64_at(bytes, ph + 8)? as usize;
        let p_filesz = u64_at(bytes, ph + 32)? as usize;

        match p_type {
            PT_INTERP => has_interp = true,
            PT_DYNAMIC => dynamic = Some((p_offset, p_filesz)),
            _ => {}
        }
    }

    let mut has_pie_flag = false;
    let mut needed_offsets = Vec::new();
    let mut strtab_vaddr = None;

    if let Some((offset, size)) = dynamic {
        let end = offset + size;
        let mut cursor = offset;
        while cursor + 16 <= end && cursor + 16 <= bytes.len() {
            let tag = u64_at(bytes, cursor)?;
            let val = u64_at(bytes, cursor + 8)?;
            match tag {
                DT_NULL => break,
                DT_FLAGS_1 => has_pie_flag = val & DF_1_PIE != 0,
                DT_NEEDED => needed_offsets.push(val),
                DT_STRTAB => strtab_vaddr = Some(val),
                _ => {}
            }
            cursor += 16;
        }
    }

    let needed = match strtab_vaddr {
        Some(vaddr) if !needed_offsets.is_empty() => {
            let strtab = vaddr_to_offset(bytes, vaddr, e_phoff, e_phentsize, e_phnum)?;
            needed_offsets
                .iter()
                .filter_map(|&off| read_cstr(bytes, strtab + off as usize))
                .collect()
        }
        _ => Vec::new(),
    };

    Ok(ElfInfo {
        is_dyn: e_type == ET_DYN,
        has_pie_flag,
        has_interp,
        needed,
    })
}

/// The dynamic string table is addressed by virtual address, so it has to be
/// mapped back through the PT_LOAD segments to a file offset.
fn vaddr_to_offset(
    bytes: &[u8],
    vaddr: u64,
    e_phoff: usize,
    e_phentsize: usize,
    e_phnum: usize,
) -> Result<usize> {
    const PT_LOAD: u32 = 1;
    for i in 0..e_phnum {
        let ph = e_phoff + i * e_phentsize;
        if u32_at(bytes, ph)? != PT_LOAD {
            continue;
        }
        let p_offset = u64_at(bytes, ph + 8)?;
        let p_vaddr = u64_at(bytes, ph + 16)?;
        let p_filesz = u64_at(bytes, ph + 32)?;
        if vaddr >= p_vaddr && vaddr < p_vaddr + p_filesz {
            return Ok((vaddr - p_vaddr + p_offset) as usize);
        }
    }
    bail!("could not map vaddr {vaddr:#x} to a file offset")
}

fn read_cstr(bytes: &[u8], start: usize) -> Option<String> {
    let rest = bytes.get(start..)?;
    let end = rest.iter().position(|&b| b == 0)?;
    std::str::from_utf8(&rest[..end]).ok().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal but structurally valid ELF64 image so the parser is
    /// exercised end to end — program headers, dynamic section, and the
    /// vaddr→offset mapping used to reach the string table.
    struct ElfBuilder {
        e_type: u16,
        interp: bool,
        flags_1: Option<u64>,
        needed: Vec<&'static str>,
    }

    impl ElfBuilder {
        fn pie() -> Self {
            Self {
                e_type: ET_DYN,
                interp: true,
                flags_1: Some(DF_1_PIE),
                needed: vec!["libc.so.6"],
            }
        }

        fn build(&self) -> Vec<u8> {
            const PHDR_OFF: usize = 64;
            const PHDR_SIZE: usize = 56;
            const INTERP_OFF: usize = 232;
            const DYN_OFF: usize = 320;
            const STRTAB_OFF: usize = 640;

            let mut strtab = vec![0u8];
            let mut needed_offsets = Vec::new();
            for lib in &self.needed {
                needed_offsets.push(strtab.len() as u64);
                strtab.extend_from_slice(lib.as_bytes());
                strtab.push(0);
            }

            let mut dynamic: Vec<(u64, u64)> = Vec::new();
            for offset in &needed_offsets {
                dynamic.push((DT_NEEDED, *offset));
            }
            dynamic.push((DT_STRTAB, STRTAB_OFF as u64));
            if let Some(flags) = self.flags_1 {
                dynamic.push((DT_FLAGS_1, flags));
            }
            dynamic.push((DT_NULL, 0));

            let total = STRTAB_OFF + strtab.len();
            let mut image = vec![0u8; total];

            image[0..4].copy_from_slice(b"\x7fELF");
            image[4] = 2; // ELFCLASS64
            image[5] = 1; // ELFDATA2LSB
            image[6] = 1; // EV_CURRENT
            image[16..18].copy_from_slice(&self.e_type.to_le_bytes());
            image[32..40].copy_from_slice(&(PHDR_OFF as u64).to_le_bytes());
            image[54..56].copy_from_slice(&(PHDR_SIZE as u16).to_le_bytes());

            let mut phdrs: Vec<(u32, u64, u64)> = vec![
                // PT_LOAD covering the whole image, mapping vaddr == offset.
                (1, 0, total as u64),
                (PT_DYNAMIC, DYN_OFF as u64, (dynamic.len() * 16) as u64),
            ];
            if self.interp {
                phdrs.push((PT_INTERP, INTERP_OFF as u64, 20));
            }
            image[56..58].copy_from_slice(&(phdrs.len() as u16).to_le_bytes());

            for (index, (p_type, offset, filesz)) in phdrs.iter().enumerate() {
                let base = PHDR_OFF + index * PHDR_SIZE;
                image[base..base + 4].copy_from_slice(&p_type.to_le_bytes());
                image[base + 8..base + 16].copy_from_slice(&offset.to_le_bytes());
                image[base + 16..base + 24].copy_from_slice(&offset.to_le_bytes());
                image[base + 32..base + 40].copy_from_slice(&filesz.to_le_bytes());
            }

            image[INTERP_OFF..INTERP_OFF + 19].copy_from_slice(b"/lib64/ld-linux.so\0");

            for (index, (tag, value)) in dynamic.iter().enumerate() {
                let base = DYN_OFF + index * 16;
                image[base..base + 8].copy_from_slice(&tag.to_le_bytes());
                image[base + 8..base + 16].copy_from_slice(&value.to_le_bytes());
            }

            image[STRTAB_OFF..STRTAB_OFF + strtab.len()].copy_from_slice(&strtab);
            image
        }
    }

    #[test]
    fn accepts_a_real_pie_executable() {
        let info = parse(&ElfBuilder::pie().build()).expect("parses");
        assert!(info.is_dyn);
        assert!(info.has_pie_flag);
        assert!(info.has_interp);
        assert!(info.is_pie_executable());
        assert_eq!(info.needed, vec!["libc.so.6".to_string()]);
    }

    /// The case the whole verification exists for: a shared library is also
    /// ET_DYN, so type alone would wrongly pass it.
    #[test]
    fn rejects_a_shared_library() {
        let mut builder = ElfBuilder::pie();
        builder.interp = false;
        let info = parse(&builder.build()).expect("parses");
        assert!(info.is_dyn, "a shared library is still ET_DYN");
        assert!(!info.is_pie_executable());
        assert!(info.explain_failure().contains("PT_INTERP"));
    }

    #[test]
    fn rejects_a_fixed_address_executable() {
        let mut builder = ElfBuilder::pie();
        builder.e_type = 2; // ET_EXEC
        builder.flags_1 = None;
        let info = parse(&builder.build()).expect("parses");
        assert!(!info.is_pie_executable());
        assert!(info.explain_failure().contains("DYN"));
    }

    /// DT_FLAGS_1 present but without the PIE bit must not count.
    #[test]
    fn rejects_dyn_without_the_pie_bit() {
        let mut builder = ElfBuilder::pie();
        builder.flags_1 = Some(0x0000_0001); // DF_1_NOW
        let info = parse(&builder.build()).expect("parses");
        assert!(info.is_dyn && info.has_interp);
        assert!(!info.has_pie_flag);
        assert!(!info.is_pie_executable());
    }

    #[test]
    fn reads_every_needed_library() {
        let mut builder = ElfBuilder::pie();
        builder.needed = vec!["libc.so.6", "libm.so.6", "libssl.so.3"];
        let info = parse(&builder.build()).expect("parses");
        assert_eq!(info.needed, vec!["libc.so.6", "libm.so.6", "libssl.so.3"]);
    }

    #[test]
    fn rejects_32_bit_and_big_endian() {
        let mut image = ElfBuilder::pie().build();
        image[4] = 1; // ELFCLASS32
        assert!(parse(&image).is_err());

        let mut image = ElfBuilder::pie().build();
        image[5] = 2; // ELFDATA2MSB
        assert!(parse(&image).is_err());
    }

    #[test]
    fn rejects_non_elf() {
        assert!(parse(&[0u8; 128]).is_err());
    }

    #[test]
    fn rejects_truncated() {
        assert!(parse(b"\x7fELF").is_err());
    }

    #[test]
    fn failure_explanation_lists_every_reason() {
        let info = ElfInfo {
            is_dyn: false,
            has_pie_flag: false,
            has_interp: false,
            needed: vec![],
        };
        assert!(!info.is_pie_executable());
        assert_eq!(info.explain_failure().matches(';').count(), 2);
    }
}
