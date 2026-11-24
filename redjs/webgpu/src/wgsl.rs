//! Naga 30 validates and lowers subgroup operations, but rejects their standard enable directive.
use std::{borrow::Cow, ops::Range};

/// Read one token of the directive preamble, preserving source byte offsets.
fn token(text: &str, pos: &mut usize) -> Option<Range<usize>> {
    let bytes = text.as_bytes();
    loop {
        let rest = text.get(*pos..)?;
        let c = rest.chars().next()?;
        if c.is_whitespace() || matches!(c, '\u{200e}' | '\u{200f}') {
            *pos += c.len_utf8();
        } else if rest.starts_with("//") {
            *pos += rest.find(['\r', '\n']).unwrap_or(rest.len());
        } else if rest.starts_with("/*") {
            *pos += 2;
            let mut depth = 1;
            while depth != 0 {
                match bytes.get(*pos..pos.checked_add(2)?) {
                    Some(b"/*") => {
                        depth += 1;
                        *pos += 2;
                    }
                    Some(b"*/") => {
                        depth -= 1;
                        *pos += 2;
                    }
                    Some(_) => *pos += 1,
                    None => return None,
                }
            }
        } else {
            let start = *pos;
            *pos += c.len_utf8();
            if c.is_ascii_alphanumeric() || c == '_' {
                while bytes
                    .get(*pos)
                    .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
                {
                    *pos += 1;
                }
            }
            return Some(start..*pos);
        }
    }
}

pub(super) fn for_device(text: &str, features: wgpu::Features) -> Cow<'_, str> {
    if !features.contains(wgpu::Features::SUBGROUP) || !text.contains("subgroups") {
        return Cow::Borrowed(text);
    }
    let mut pos = 0;
    let mut blank = Vec::new();
    while let Some(start) = token(text, &mut pos) {
        let directive = &text[start.clone()];
        if !matches!(directive, "enable" | "requires" | "diagnostic") {
            break;
        }
        let mut body = Vec::new();
        let end = loop {
            let Some(t) = token(text, &mut pos) else {
                return Cow::Borrowed(text);
            };
            if &text[t.clone()] == ";" {
                break t;
            }
            body.push(t);
        };
        if directive != "enable" || !body.iter().any(|t| &text[t.clone()] == "subgroups") {
            continue;
        }
        // Leave malformed directives to Naga. A trailing comma is permitted.
        if !body.iter().enumerate().all(|(i, t)| {
            let word = &text[t.clone()];
            if i % 2 == 1 {
                word == ","
            } else {
                word.as_bytes()[0].is_ascii_alphabetic() || word.starts_with('_')
            }
        }) {
            continue;
        }
        let mut kept = false;
        for pair in body.chunks(2).rev() {
            let subgroup = &text[pair[0].clone()] == "subgroups";
            if let Some(comma) = pair.get(1) {
                if subgroup || !kept {
                    blank.push(comma.clone());
                }
            }
            if subgroup {
                blank.push(pair[0].clone());
            } else {
                kept = true;
            }
        }
        if !kept {
            blank.extend([start, end]);
        }
    }
    if blank.is_empty() {
        return Cow::Borrowed(text);
    }
    let mut code = text.as_bytes().to_vec();
    // Only ASCII directive tokens are blanked. Diagnostics retain line and UTF-16 positions.
    for range in blank {
        code[range].fill(b' ');
    }
    Cow::Owned(String::from_utf8(code).unwrap())
}
