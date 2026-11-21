use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

const HEADER_SIZE: usize = 32;
const MAX_COMMANDS_SIZE: usize = 16384 - HEADER_SIZE;

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "expected a signed arm64 Mach-O image container without initializers or extra libraries",
    )
}

fn word(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn name(bytes: &[u8]) -> &[u8] {
    &bytes[..bytes
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(bytes.len())]
}

fn validate_commands(mut commands: &[u8], count: u32) -> Option<(u32, u32)> {
    let mut segments = 0;
    let mut signed = false;
    let mut exports = None;
    for _ in 0..count {
        let length = word(commands, 4)? as usize;
        if length < 8 || length % 8 != 0 {
            return None;
        }
        let command = commands.get(..length)?;
        commands = &commands[length..];
        match word(command, 0)? {
            0x19 => {
                let segment = name(command.get(8..24)?);
                let bit = match segment {
                    b"__TEXT" => 1,
                    b"__SKHDR" => 2,
                    b"__SKCODE" => 4,
                    b"__SKDATA" => 8,
                    b"__LINKEDIT" => 16,
                    _ => return None,
                };
                if segments & bit != 0 {
                    return None;
                }
                segments |= bit;
                let sections = command.get(72..)?;
                if sections.len() % 80 != 0 || word(command, 64)? as usize != sections.len() / 80 {
                    return None;
                }
                for section in sections.chunks_exact(80) {
                    if name(&section[16..32]) != segment
                        || word(section, 64)? & 0xff != 0
                        || word(section, 60)? != 0
                    {
                        return None;
                    }
                    match (segment, name(&section[..16])) {
                        (b"__TEXT", b"__text" | b"__const" | b"__unwind_info")
                        | (b"__SKHDR" | b"__SKDATA", b"__aot_data")
                        | (b"__SKCODE", b"__aot_code") => {}
                        _ => return None,
                    }
                }
            }
            0x1d => {
                if signed || word(command, 8)? == 0 || word(command, 12)? == 0 {
                    return None;
                }
                signed = true;
            }
            0xc => {
                // Current dyld requires at least one linked library. This path is in the shared cache.
                let offset = word(command, 8)? as usize;
                if offset < 24 || name(command.get(offset..)?) != b"/usr/lib/libSystem.B.dylib" {
                    return None;
                }
            }
            0x22 | 0x8000_0022 => {
                // Only the export trie is needed. Rebasing, binding and lazy binding are forbidden.
                if command.get(8..40)?.iter().any(|&byte| byte != 0) {
                    return None;
                }
                if word(command, 44)? != 0 {
                    if exports.is_some() {
                        return None;
                    }
                    exports = Some((word(command, 40)?, word(command, 44)?));
                }
            }
            0x8000_0034 => {
                if word(command, 12)? != 0 {
                    return None;
                }
            }
            0xb => {
                // Apple ld lists dyld_stub_binder as undefined even when nothing binds to it.
                if command.get(32..80)?.iter().any(|&byte| byte != 0) {
                    return None;
                }
            }
            0x8000_0033 => {
                if exports.is_some() {
                    return None;
                }
                exports = Some((word(command, 8)?, word(command, 12)?));
            }
            0x2 | 0xd | 0x1b | 0x24 | 0x26 | 0x29 | 0x2a | 0x2e | 0x32 => {}
            _ => return None,
        }
    }
    // __SKDATA is optional when the executable portion ends at the end of the image.
    if commands.is_empty() && signed && segments & 0x17 == 0x17 {
        exports
    } else {
        None
    }
}

fn uleb(input: &mut &[u8]) -> Option<usize> {
    let mut value = 0;
    for shift in (0..28).step_by(7) {
        let (&byte, rest) = input.split_first()?;
        *input = rest;
        value |= usize::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

fn anchor_is_data(trie: &[u8]) -> Option<()> {
    let mut key = b"_skathi_image_anchor".as_slice();
    let mut node = trie;
    loop {
        let terminal_size = uleb(&mut node)?;
        let mut terminal = node.get(..terminal_size)?;
        if key.is_empty() {
            // dlsym may execute an exported resolver. The anchor must be a plain address in __TEXT.
            return (uleb(&mut terminal)? == 0
                && uleb(&mut terminal)? < MAX_COMMANDS_SIZE + HEADER_SIZE
                && terminal.is_empty())
            .then_some(());
        }
        let (&children, mut edges) = node.get(terminal_size..)?.split_first()?;
        let mut found = None;
        for _ in 0..children {
            let end = edges.iter().position(|&byte| byte == 0)?;
            let edge = edges.get(..end)?;
            edges = edges.get(end + 1..)?;
            let offset = uleb(&mut edges)?;
            if !edge.is_empty() && key.starts_with(edge) {
                if found.is_some() {
                    return None;
                }
                found = Some((offset, end));
            }
        }
        let (offset, consumed) = found?;
        key = key.get(consumed..)?;
        node = trie.get(offset..)?;
    }
}

pub(super) fn preflight(path: &Path) -> io::Result<()> {
    let mut file = File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(invalid());
    }
    let mut header = [0; HEADER_SIZE];
    file.read_exact(&mut header)?;
    if word(&header, 0) != Some(0xfeed_facf)
        || word(&header, 4) != Some(0x0100_000c)
        || word(&header, 8) != Some(0)
        || word(&header, 12) != Some(6)
    {
        return Err(invalid());
    }
    let size = word(&header, 20).ok_or_else(invalid)? as usize;
    let count = word(&header, 16).ok_or_else(invalid)?;
    if size > MAX_COMMANDS_SIZE {
        return Err(invalid());
    }
    let mut commands = vec![0; size];
    file.read_exact(&mut commands)?;
    let (exports, size) = validate_commands(&commands, count).ok_or_else(invalid)?;
    if exports == 0 || size == 0 || size > 4096 {
        return Err(invalid());
    }
    file.seek(SeekFrom::Start(u64::from(exports)))?;
    let mut trie = vec![0; size as usize];
    file.read_exact(&mut trie)?;
    anchor_is_data(&trie).ok_or_else(invalid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(segment: &[u8], section: &[u8], flags: u32) -> Vec<u8> {
        let mut out = vec![0; if section.is_empty() { 72 } else { 152 }];
        let len = out.len() as u32;
        out[0..4].copy_from_slice(&0x19u32.to_le_bytes());
        out[4..8].copy_from_slice(&len.to_le_bytes());
        out[8..8 + segment.len()].copy_from_slice(segment);
        if !section.is_empty() {
            out[64] = 1;
            out[72..72 + section.len()].copy_from_slice(section);
            out[88..88 + segment.len()].copy_from_slice(segment);
            out[136..140].copy_from_slice(&flags.to_le_bytes());
        }
        out
    }

    fn container(text: Vec<u8>) -> Vec<u8> {
        [
            text,
            segment(b"__SKHDR", b"__aot_data", 0),
            segment(b"__SKCODE", b"__aot_code", 0x8000_0400),
            segment(b"__LINKEDIT", b"", 0),
            vec![0x1d, 0, 0, 0, 16, 0, 0, 0, 0, 1, 0, 0, 16, 0, 0, 0],
            vec![0x33, 0, 0, 0x80, 16, 0, 0, 0, 0, 2, 0, 0, 32, 0, 0, 0],
        ]
        .concat()
    }

    #[test]
    fn permits_only_passive_image_containers() {
        let mut good = container(segment(b"__TEXT", b"__const", 0));
        assert_eq!(validate_commands(&good, 6), Some((512, 32)));
        for cmd in [0xc, 0x1a, 0x1c] {
            good.extend_from_slice(&[cmd, 0, 0, 0, 8, 0, 0, 0]);
            assert_eq!(validate_commands(&good, 7), None);
            good.truncate(good.len() - 8);
        }
        for flags in [0x09, 0x0d, 0x15, 0x16] {
            let active = container(segment(b"__TEXT", b"__const", flags));
            assert_eq!(validate_commands(&active, 6), None);
        }
        let objc = container(segment(b"__TEXT", b"__objc_methlist", 0));
        assert_eq!(validate_commands(&objc, 6), None);
    }

    #[test]
    fn permits_the_unused_undefined_symbol_from_apple_ld() {
        let mut commands = container(segment(b"__TEXT", b"__const", 0));
        let start = commands.len();
        let mut dy_symtab = [0; 80];
        dy_symtab[0] = 0xb;
        dy_symtab[4] = 80;
        dy_symtab[20] = 1;
        dy_symtab[24] = 1;
        dy_symtab[28] = 1;
        commands.extend_from_slice(&dy_symtab);
        assert_eq!(validate_commands(&commands, 7), Some((512, 32)));

        for offset in [56, 60, 64, 68, 72, 76] {
            commands[start + offset] = 1;
            assert_eq!(validate_commands(&commands, 7), None);
            commands[start + offset] = 0;
        }

        let end = commands.len();
        let mut dyld_info = [0; 48];
        dyld_info[..4].copy_from_slice(&0x8000_0022u32.to_le_bytes());
        dyld_info[4] = 48;
        commands.extend_from_slice(&dyld_info);
        for offset in [12, 20, 28, 36] {
            commands[end + offset] = 1;
            assert_eq!(validate_commands(&commands, 8), None);
            commands[end + offset] = 0;
        }
        commands.truncate(end);
        commands.extend_from_slice(&[0x34, 0, 0, 0x80, 16, 0, 0, 0, 0, 2, 0, 0, 32, 0, 0, 0]);
        assert_eq!(validate_commands(&commands, 8), None);
    }

    #[test]
    fn the_payload_anchor_must_not_be_a_symbol_resolver() {
        let mut trie = vec![0, 1];
        trie.extend_from_slice(b"_skathi_image_anchor\0");
        trie.push((trie.len() + 1) as u8);
        let terminal = trie.len();
        trie.extend_from_slice(&[2, 0, 42, 0]);
        assert_eq!(anchor_is_data(&trie), Some(()));
        trie[terminal + 1] = 0x10;
        assert_eq!(anchor_is_data(&trie), None);
    }
}
