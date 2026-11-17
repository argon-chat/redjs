//! `icu-root`: links a root-only subset of the prebuilt's ICU common data instead of all of it.
//!
//! The prebuilt's data library (`sicudt.lib` / `libicudata.a`) holds one object defining
//! `icudt<NN>_dat` (the common-data package, items zstd-compressed by Bun), `bun_icu_zstd_dict`
//! and `bun_icu_zstd_dict_size`. The subset keeps the items `icu-root.txt` names, byte for byte,
//! and the same dictionary, so the full package stays usable at run time
//! (`redjs::icu::set_common_data`). The subset's sha256 is pinned in `pins.txt`.

use std::fs;
use std::path::Path;

use sha2::{Digest, Sha256};

pub struct IcuData {
    /// `icudt79_dat`: the linker symbol of the package.
    pub symbol: String,
    /// `icudt79l`: the TOC name prefix and the package's file stem.
    pub package: String,
    pub data: Vec<u8>,
    pub dict: Vec<u8>,
}

pub fn read(library: &Path, windows: bool) -> IcuData {
    let bytes = fs::read(library).unwrap_or_else(|e| panic!("reading {}: {e}", library.display()));
    let object = archive_members(&bytes)
        .into_iter()
        .find(|(name, _)| name.contains("_dat"))
        .unwrap_or_else(|| panic!("{} has no ICU data object", library.display()))
        .1;
    let (section, symbols) = if windows {
        coff_symbols(object)
    } else {
        elf_symbols(object)
    };
    let at = |name: &str| {
        symbols
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, offset)| *offset)
            .unwrap_or_else(|| panic!("{} defines no {name}", library.display()))
    };
    let (symbol, start) = symbols
        .iter()
        .find(|(n, _)| n.starts_with("icudt") && n.ends_with("_dat"))
        .cloned()
        .unwrap_or_else(|| panic!("{} defines no icudt<NN>_dat", library.display()));
    let dict = at("bun_icu_zstd_dict");
    let size_at = at("bun_icu_zstd_dict_size");
    let size = u32::from_le_bytes(section[size_at..size_at + 4].try_into().unwrap()) as usize;
    let data = section[start..dict].to_vec();
    let package = toc(&data)
        .first()
        .and_then(|(name, _)| name.split_once('/'))
        .map(|(prefix, _)| prefix.to_owned())
        .expect("the ICU package has no items");
    IcuData {
        symbol,
        package,
        data,
        dict: section[dict..dict + size].to_vec(),
    }
}

/// The package's items: (full TOC name, offset from the package start), in TOC order.
fn toc(data: &[u8]) -> Vec<(String, usize)> {
    let header = u16::from_le_bytes([data[0], data[1]]) as usize;
    assert!(
        data[2] == 0xda && data[3] == 0x27,
        "not an ICU data package"
    );
    assert_eq!(&data[12..16], b"CmnD", "not an ICU common-data package");
    let count = u32_at(data, header) as usize;
    (0..count)
        .map(|i| {
            let entry = header + 4 + 8 * i;
            let name = header + u32_at(data, entry) as usize;
            let end = name + data[name..].iter().position(|&b| b == 0).unwrap();
            (
                String::from_utf8(data[name..end].to_vec()).unwrap(),
                header + u32_at(data, entry + 4) as usize,
            )
        })
        .collect()
}

/// A package of the items `keep` names (without the package prefix), as stored in `full`.
pub fn subset(full: &IcuData, keep: &[&str]) -> Vec<u8> {
    let items = toc(&full.data);
    let header = u16::from_le_bytes([full.data[0], full.data[1]]) as usize;
    let mut chosen = Vec::new();
    for want in keep {
        let name = format!("{}/{want}", full.package);
        let i = items
            .iter()
            .position(|(n, _)| *n == name)
            .unwrap_or_else(|| {
                panic!("icu-root.txt names {want}, which the prebuilt's ICU data does not have")
            });
        let end = items
            .get(i + 1)
            .map_or(full.data.len(), |(_, offset)| *offset);
        chosen.push((name, &full.data[items[i].1..end]));
    }
    // ICU finds items by binary search over the TOC names.
    chosen.sort_by(|a, b| a.0.cmp(&b.0));

    let names_at = 4 + 8 * chosen.len();
    let names_len: usize = chosen.iter().map(|(n, _)| n.len() + 1).sum();
    let mut out = full.data[..header].to_vec();
    out.extend((chosen.len() as u32).to_le_bytes());
    let mut name = names_at;
    let mut item = align(header + names_at + names_len, 16) - header;
    for (n, bytes) in &chosen {
        out.extend((name as u32).to_le_bytes());
        out.extend((item as u32).to_le_bytes());
        name += n.len() + 1;
        item = align(header + item + bytes.len(), 16) - header;
    }
    for (n, _) in &chosen {
        out.extend(n.as_bytes());
        out.push(0);
    }
    for (_, bytes) in &chosen {
        out.resize(align(out.len(), 16), 0);
        out.extend(*bytes);
    }
    out
}

pub fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// An object file defining `symbol` (the package), `bun_icu_zstd_dict` and `bun_icu_zstd_dict_size`.
pub fn object(symbol: &str, package: &[u8], dict: &[u8], windows: bool, arch: &str) -> Vec<u8> {
    let mut section = package.to_vec();
    section.resize(align(section.len(), 16), 0);
    let dict_at = section.len();
    section.extend(dict);
    section.resize(align(section.len(), 4), 0);
    let size_at = section.len();
    section.extend((dict.len() as u32).to_le_bytes());
    let symbols = [
        (symbol, 0, package.len()),
        ("bun_icu_zstd_dict", dict_at, dict.len()),
        ("bun_icu_zstd_dict_size", size_at, 4),
    ];
    if windows {
        coff(&section, &symbols)
    } else {
        elf(&section, &symbols, arch)
    }
}

fn coff(section: &[u8], symbols: &[(&str, usize, usize)]) -> Vec<u8> {
    const HEADERS: usize = 20 + 40;
    let symbol_table = HEADERS + section.len();
    let mut out = Vec::with_capacity(symbol_table + 18 * symbols.len() + 64);
    out.extend(0x8664u16.to_le_bytes());
    out.extend(1u16.to_le_bytes());
    out.extend(0u32.to_le_bytes()); // no timestamp: the same inputs give the same object
    out.extend((symbol_table as u32).to_le_bytes());
    out.extend((symbols.len() as u32).to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out.extend(0u16.to_le_bytes());

    out.extend(b".rdata\0\0");
    out.extend(0u32.to_le_bytes());
    out.extend(0u32.to_le_bytes());
    out.extend((section.len() as u32).to_le_bytes());
    out.extend((HEADERS as u32).to_le_bytes());
    out.extend([0u8; 12]);
    // IMAGE_SCN_CNT_INITIALIZED_DATA | IMAGE_SCN_ALIGN_16BYTES | IMAGE_SCN_MEM_READ
    out.extend(0x4050_0040u32.to_le_bytes());
    out.extend(section);

    let mut strings = Vec::new();
    for (name, offset, _) in symbols {
        if name.len() <= 8 {
            let mut short = [0u8; 8];
            short[..name.len()].copy_from_slice(name.as_bytes());
            out.extend(short);
        } else {
            out.extend(0u32.to_le_bytes());
            out.extend((4 + strings.len() as u32).to_le_bytes());
            strings.extend(name.as_bytes());
            strings.push(0);
        }
        out.extend((*offset as u32).to_le_bytes());
        out.extend(1i16.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.push(2); // IMAGE_SYM_CLASS_EXTERNAL
        out.push(0);
    }
    out.extend((4 + strings.len() as u32).to_le_bytes());
    out.extend(strings);
    out
}

fn elf(section: &[u8], symbols: &[(&str, usize, usize)], arch: &str) -> Vec<u8> {
    let machine: u16 = match arch {
        "x86_64" => 62,
        "aarch64" => 183,
        _ => panic!("icu-root has no ELF machine for {arch}"),
    };
    let shstrtab = b"\0.rodata\0.note.GNU-stack\0.symtab\0.strtab\0.shstrtab\0";
    let name = |s: &str| {
        shstrtab
            .windows(s.len())
            .position(|w| w == s.as_bytes())
            .unwrap() as u32
    };
    let mut strtab = vec![0u8];
    let mut symtab = vec![0u8; 24];
    for (symbol, offset, size) in symbols {
        symtab.extend((strtab.len() as u32).to_le_bytes());
        symtab.push(0x11); // STB_GLOBAL, STT_OBJECT
        symtab.push(0);
        symtab.extend(1u16.to_le_bytes());
        symtab.extend((*offset as u64).to_le_bytes());
        symtab.extend((*size as u64).to_le_bytes());
        strtab.extend(symbol.as_bytes());
        strtab.push(0);
    }

    let rodata_at = 64;
    let symtab_at = align(rodata_at + section.len(), 8);
    let strtab_at = symtab_at + symtab.len();
    let shstrtab_at = strtab_at + strtab.len();
    let headers_at = align(shstrtab_at + shstrtab.len(), 8);

    let mut out = Vec::with_capacity(headers_at + 6 * 64);
    out.extend(b"\x7fELF\x02\x01\x01\0\0\0\0\0\0\0\0\0");
    out.extend(1u16.to_le_bytes()); // ET_REL
    out.extend(machine.to_le_bytes());
    out.extend(1u32.to_le_bytes());
    out.extend(0u64.to_le_bytes());
    out.extend(0u64.to_le_bytes());
    out.extend((headers_at as u64).to_le_bytes());
    out.extend(0u32.to_le_bytes());
    out.extend(64u16.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out.extend(64u16.to_le_bytes());
    out.extend(6u16.to_le_bytes());
    out.extend(5u16.to_le_bytes());
    out.extend(section);
    out.resize(symtab_at, 0);
    out.extend(&symtab);
    out.extend(&strtab);
    out.extend(shstrtab);
    out.resize(headers_at, 0);

    let mut header = |name: u32,
                      kind: u32,
                      flags: u64,
                      offset: usize,
                      size: usize,
                      link: u32,
                      info: u32,
                      align: u64,
                      entsize: u64| {
        out.extend(name.to_le_bytes());
        out.extend(kind.to_le_bytes());
        out.extend(flags.to_le_bytes());
        out.extend(0u64.to_le_bytes());
        out.extend((offset as u64).to_le_bytes());
        out.extend((size as u64).to_le_bytes());
        out.extend(link.to_le_bytes());
        out.extend(info.to_le_bytes());
        out.extend(align.to_le_bytes());
        out.extend(entsize.to_le_bytes());
    };
    header(0, 0, 0, 0, 0, 0, 0, 0, 0);
    header(name(".rodata"), 1, 2, rodata_at, section.len(), 0, 0, 16, 0);
    header(
        name(".note.GNU-stack"),
        1,
        0,
        rodata_at + section.len(),
        0,
        0,
        0,
        1,
        0,
    );
    header(name(".symtab"), 2, 0, symtab_at, symtab.len(), 4, 1, 8, 24);
    header(name(".strtab"), 3, 0, strtab_at, strtab.len(), 0, 0, 1, 0);
    header(
        name(".shstrtab"),
        3,
        0,
        shstrtab_at,
        shstrtab.len(),
        0,
        0,
        1,
        0,
    );
    out
}

/// The members of an `ar` archive (GNU or MSVC `.lib`), less the symbol tables.
fn archive_members(bytes: &[u8]) -> Vec<(String, &[u8])> {
    assert!(bytes.starts_with(b"!<arch>\n"), "not an ar archive");
    let mut members = Vec::new();
    let mut long_names: &[u8] = &[];
    let mut at = 8;
    while at + 60 <= bytes.len() {
        let header = &bytes[at..at + 60];
        let raw = std::str::from_utf8(&header[..16]).unwrap().trim_end();
        let size: usize = std::str::from_utf8(&header[48..58])
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let body = &bytes[at + 60..at + 60 + size];
        at = align(at + 60 + size, 2);
        let name = if raw == "/" || raw == "/SYM64/" {
            continue;
        } else if raw == "//" {
            long_names = body;
            continue;
        } else if let Some(offset) = raw.strip_prefix('/').and_then(|o| o.parse::<usize>().ok()) {
            let rest = &long_names[offset..];
            let end = rest
                .iter()
                .position(|&b| b == b'\n' || b == 0)
                .unwrap_or(rest.len());
            String::from_utf8_lossy(&rest[..end])
                .trim_end_matches('/')
                .to_owned()
        } else {
            raw.trim_end_matches('/').to_owned()
        };
        members.push((name, body));
    }
    members
}

/// The one data section of a COFF object and its external symbols' offsets in it.
fn coff_symbols(object: &[u8]) -> (&[u8], Vec<(String, usize)>) {
    let sections = u16_at(object, 2) as usize;
    let table = u32_at(object, 8) as usize;
    let count = u32_at(object, 12) as usize;
    let optional = u16_at(object, 16) as usize;
    let strings = table + 18 * count;
    let section_of = |number: usize| {
        let header = 20 + optional + 40 * (number - 1);
        let size = u32_at(object, header + 16) as usize;
        let at = u32_at(object, header + 20) as usize;
        &object[at..at + size]
    };
    let mut data = None;
    let mut symbols = Vec::new();
    let mut i = 0;
    while i < count {
        let entry = table + 18 * i;
        let name = if u32_at(object, entry) == 0 {
            let at = strings + u32_at(object, entry + 4) as usize;
            let end = at + object[at..].iter().position(|&b| b == 0).unwrap();
            String::from_utf8_lossy(&object[at..end]).into_owned()
        } else {
            let raw = &object[entry..entry + 8];
            String::from_utf8_lossy(&raw[..raw.iter().position(|&b| b == 0).unwrap_or(8)])
                .into_owned()
        };
        let number = u16_at(object, entry + 12) as i16;
        if object[entry + 16] == 2 && number > 0 && number as usize <= sections {
            data.get_or_insert(number as usize);
            assert_eq!(
                data,
                Some(number as usize),
                "the ICU data object spreads over sections"
            );
            symbols.push((name, u32_at(object, entry + 8) as usize));
        }
        i += 1 + object[entry + 17] as usize;
    }
    (
        section_of(data.expect("the ICU data object has no symbols")),
        symbols,
    )
}

/// The one data section of an ELF64 object and its global symbols' offsets in it.
fn elf_symbols(object: &[u8]) -> (&[u8], Vec<(String, usize)>) {
    assert!(
        object.starts_with(b"\x7fELF\x02\x01"),
        "not a little-endian ELF64 object"
    );
    let headers = u64_at(object, 40) as usize;
    let count = u16_at(object, 60) as usize;
    let header = |i: usize| headers + 64 * i;
    let range = |i: usize| {
        let at = u64_at(object, header(i) + 24) as usize;
        at..at + u64_at(object, header(i) + 32) as usize
    };
    let symtab = (0..count)
        .find(|&i| u32_at(object, header(i) + 4) == 2)
        .expect("no symbol table");
    let strtab = range(u32_at(object, header(symtab) + 40) as usize);
    let mut data = None;
    let mut symbols = Vec::new();
    for entry in range(symtab).step_by(24).skip(1) {
        let section = u16_at(object, entry + 6) as usize;
        if object[entry + 4] >> 4 != 1 || section == 0 || section >= count {
            continue;
        }
        data.get_or_insert(section);
        assert_eq!(
            data,
            Some(section),
            "the ICU data object spreads over sections"
        );
        let at = strtab.start + u32_at(object, entry) as usize;
        let end = at + object[at..].iter().position(|&b| b == 0).unwrap();
        symbols.push((
            String::from_utf8_lossy(&object[at..end]).into_owned(),
            u64_at(object, entry + 8) as usize,
        ));
    }
    (
        &object[range(data.expect("the ICU data object has no symbols"))],
        symbols,
    )
}

fn align(n: usize, to: usize) -> usize {
    n.div_ceil(to) * to
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(b[at..at + 2].try_into().unwrap())
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}
