//! Read-only, server-owned IndexedDB inspection. No JS values, connections,
//! upgrades or transactions are created by this API.
use std::{ops::Bound, sync::mpsc::{self, Receiver}, time::{Duration, Instant}};
use super::{backend::{Database, DbMeta}, key, keypath::KeyPath, server};

#[derive(Debug)]
pub enum Query {
    List,
    Schema { database: Vec<u16> },
    Records { database: Vec<u16>, store: String, index: Option<String>, after: Option<Vec<u8>>, limit: usize, search: String },
}
impl Query { pub(super) fn database(&self) -> Option<&[u16]> { match self { Self::List => None, Self::Schema { database } | Self::Records { database, .. } => Some(database) } } }
#[derive(Debug)]
pub struct DatabaseInfo { pub name: String, pub version: u64 }
#[derive(Debug)]
pub struct IndexInfo { pub name: String, pub key_path: KeyPath, pub unique: bool, pub multi_entry: bool, pub count: u64 }
#[derive(Debug)]
pub struct StoreInfo { pub name: String, pub key_path: Option<KeyPath>, pub auto_increment: bool, pub count: u64, pub indexes: Vec<IndexInfo> }
#[derive(Debug)]
pub struct Child { pub name: String, pub value: Preview }
#[derive(Debug)]
pub struct Preview { pub kind: String, pub preview: String, pub children: Vec<Child>, pub size: Option<u64>, pub mime: Option<String>, pub name: Option<String>, pub reference: Option<u64>, pub object_id: Option<u64>, pub truncated: bool }
impl Preview {
    fn leaf(kind: &str, preview: impl Into<String>) -> Self { Self { kind: kind.into(), preview: preview.into(), children: Vec::new(), size: None, mime: None, name: None, reference: None, object_id: None, truncated: false } }
    fn matches(&self, needle: &str) -> bool { self.preview.to_lowercase().contains(needle) || self.kind.to_lowercase().contains(needle) || self.children.iter().any(|child| child.name.to_lowercase().contains(needle) || child.value.matches(needle)) || self.mime.as_ref().is_some_and(|mime| mime.to_lowercase().contains(needle)) || self.name.as_ref().is_some_and(|name| name.to_lowercase().contains(needle)) }
}
#[derive(Debug)]
pub struct Record { pub key: Preview, pub primary_key: Preview, pub value: Preview, pub encoded_bytes: u64 }
#[derive(Debug)]
pub enum Response {
    List { available: bool, backend: String, databases: Vec<DatabaseInfo>, truncated: bool },
    Schema { database: String, version: u64, stores: Vec<StoreInfo>, truncated: bool },
    Records { records: Vec<Record>, next_cursor: Option<Vec<u8>>, has_more: bool, scanned: usize, truncated: bool },
}

/// Does not start the server or touch storage before the application has used
/// IndexedDB. The receiver is polled/waited off the VM/main thread.
pub fn inspect(query: Query) -> Result<Receiver<Result<Response, String>>, String> {
    let (reply, receiver) = mpsc::channel();
    if !server::send_inspection(query, reply)? { return Err("IndexedDB has not been initialized by this application".into()); }
    Ok(receiver)
}

pub(super) fn read(database: &mut dyn Database, query: Query) -> Result<Response, String> {
    database.inspection_budget(Some(2_000_000));
    let result = read_inner(database, query);
    database.inspection_budget(None);
    result
}
fn failure(error: super::backend::BackendError) -> String { format!("IndexedDB inspection failed: {error:?}") }
fn read_inner(database: &mut dyn Database, query: Query) -> Result<Response, String> {
    let original = database.meta();
    let schema_truncated = original.stores.len() > 128 || original.stores.iter().take(128).any(|store| store.indexes.len() > 128);
    let stores = match &query {
        Query::Schema { .. } => original.stores.iter().take(128).map(|store| super::backend::StoreMeta { id: store.id, name: store.name.clone(), key_path: store.key_path.clone(), auto_increment: store.auto_increment, indexes: store.indexes.iter().take(128).cloned().collect() }).collect(),
        Query::Records { store, index, .. } => original.stores.iter().filter(|candidate| text(&candidate.name) == *store).take(1).map(|store| super::backend::StoreMeta { id: store.id, name: store.name.clone(), key_path: None, auto_increment: store.auto_increment, indexes: store.indexes.iter().filter(|candidate| index.as_ref().is_some_and(|name| text(&candidate.name) == *name)).take(1).cloned().collect() }).collect(),
        Query::List => Vec::new(),
    };
    let metadata = DbMeta { name: original.name.clone(), version: original.version, stores };
    match query {
        Query::Schema { .. } => {
            let mut stores = Vec::new();
            let deadline = Instant::now() + Duration::from_millis(40);
            let mut truncated = schema_truncated;
            for store in metadata.stores.iter().take(128) {
                if Instant::now() >= deadline { truncated = true; break; }
                let count = database.count(store.id, Bound::Unbounded, Bound::Unbounded).map_err(failure)?;
                let mut indexes = Vec::new();
                for index in store.indexes.iter().take(128) {
                    if Instant::now() >= deadline { truncated = true; break; }
                    let count = database.index_count(index.id, Bound::Unbounded, Bound::Unbounded).map_err(failure)?;
                    indexes.push(IndexInfo { name: text(&index.name), key_path: index.key_path.clone(), unique: index.unique, multi_entry: index.multi_entry, count });
                }
                truncated |= store.indexes.len() > 128;
                stores.push(StoreInfo { name: text(&store.name), key_path: store.key_path.clone(), auto_increment: store.auto_increment, count, indexes });
            }
            Ok(Response::Schema { database: text(&metadata.name), version: metadata.version, stores, truncated })
        }
        Query::Records { store, index, after, limit, search, .. } => {
            let store = metadata.stores.iter().find(|candidate| text(&candidate.name) == store).ok_or("Object store does not exist")?;
            let index = index.filter(|index| !index.is_empty()).map(|name| store.indexes.iter().find(|index| text(&index.name) == name).ok_or("Index does not exist")).transpose()?;
            let limit = limit.clamp(1, 100);
            let scan_limit = if search.is_empty() { limit + 1 } else { 256 };
            let lower = after.as_deref().map(Bound::Excluded).unwrap_or(Bound::Unbounded);
            let keys = match index { Some(index) => database.index_scan(index.id, lower, Bound::Unbounded, false, scan_limit), None => database.keys(store.id, lower, Bound::Unbounded, false, scan_limit) }.map_err(failure)?;
            let mut records = Vec::new();
            let mut last = after;
            let mut scanned = 0;
            let deadline = Instant::now() + Duration::from_millis(40);
            let needle = search.chars().take(256).collect::<String>().to_lowercase();
            let mut exhausted = true;
            for encoded in &keys {
                if records.len() == limit || Instant::now() >= deadline { exhausted = false; break; }
                if encoded.len() > 8192 { return Err("An IndexedDB key exceeds the inspector's 8192-byte key budget".into()); }
                let (key, primary) = if index.is_some() { let length = key::key_len(encoded).ok_or("Corrupt index key")?; (&encoded[..length], &encoded[length..]) } else { (encoded.as_slice(), encoded.as_slice()) };
                let key = key_preview(key)?;
                let primary_key = key_preview(primary)?;
                let value = database.inspect_value(store.id, primary, 16 * 1024).map_err(failure)?;
                last = Some(encoded.clone());
                scanned += 1;
                let Some((encoded_bytes, bytes)) = value else { continue; };
                let value = preview(&bytes, encoded_bytes);
                if !needle.is_empty() && !key.matches(&needle) && !primary_key.matches(&needle) && !value.matches(&needle) { continue; }
                records.push(Record { key, primary_key, value, encoded_bytes });
            }
            let has_more = !exhausted || keys.len() == scan_limit;
            Ok(Response::Records { records, next_cursor: has_more.then_some(last).flatten(), has_more, scanned, truncated: !exhausted })
        }
        Query::List => Err("Database list is served by the server".into()),
    }
}
pub(super) fn text(value: &[u16]) -> String { String::from_utf16_lossy(value) }
fn key_preview(bytes: &[u8]) -> Result<Preview, String> {
    fn convert(key: key::Key, depth: usize) -> Preview {
        if depth > 6 { let mut output = Preview::leaf("Array", "…"); output.truncated = true; return output; }
        match key {
            key::Key::Number(value) => Preview::leaf("Number", value.to_string()),
            key::Key::Date(value) => Preview::leaf("Date", format!("Date({value})")),
            key::Key::String(value) => { let mut output = Preview::leaf("String", text(&value[..value.len().min(256)])); output.size = Some(value.len() as u64); output.truncated = value.len() > 256; output },
            key::Key::Binary(value) => { let mut output = Preview::leaf("Binary", hex(&value[..value.len().min(32)])); output.size = Some(value.len() as u64); output.truncated = value.len() > 32; output },
            key::Key::Array(value) => { let mut output = Preview::leaf("Array", format!("Array({})", value.len())); output.truncated = value.len() > 32; output.children = value.into_iter().take(32).enumerate().map(|(index, value)| Child { name: index.to_string(), value: convert(value, depth + 1) }).collect(); output },
        }
    }
    key::decode(bytes).map(|key| convert(key, 0)).ok_or_else(|| "Corrupt IndexedDB key".into())
}
pub fn hex(bytes: &[u8]) -> String { const DIGITS: &[u8] = b"0123456789abcdef"; let mut output = String::with_capacity(bytes.len() * 2); for byte in bytes { output.push(DIGITS[(byte >> 4) as usize] as char); output.push(DIGITS[(byte & 15) as usize] as char); } output }
pub fn unhex(value: &str) -> Result<Vec<u8>, String> { if value.len() > 16384 || value.len() % 2 != 0 { return Err("Invalid IndexedDB cursor".into()); } value.as_bytes().chunks_exact(2).map(|bytes| { let digit = |byte: u8| (byte as char).to_digit(16).map(|value| value as u8); Ok(digit(bytes[0]).ok_or("Invalid IndexedDB cursor")? * 16 + digit(bytes[1]).ok_or("Invalid IndexedDB cursor")?) }).collect() }

/// Parses a bounded serialized prefix, never deserializes a JS object or
/// invokes getters. Cycles remain explicit reference IDs; Blob bodies skip.
pub fn preview(bytes: &[u8], encoded_bytes: u64) -> Preview {
    if bytes.len() < 5 || bytes[..3] != crate::webapi::serialize::MAGIC || bytes[3] != crate::webapi::serialize::VERSION {
        return Preview::leaf("Invalid", "Invalid structured-clone bytes");
    }
    let mut reader = Reader { bytes, position: 5, objects: 0, nodes: 0, halted: false, shortened: false };
    let mut output = reader.value(0).unwrap_or_else(|| Preview::leaf("Invalid", "Invalid or truncated structured clone"));
    output.truncated |= reader.halted || reader.shortened || encoded_bytes > bytes.len() as u64;
    output
}
struct Reader<'a> { bytes: &'a [u8], position: usize, objects: u64, nodes: usize, halted: bool, shortened: bool }
impl Reader<'_> {
    fn byte(&mut self) -> Option<u8> { let byte = *self.bytes.get(self.position)?; self.position += 1; Some(byte) }
    fn number(&mut self) -> Option<f64> { let bytes = self.bytes.get(self.position..self.position.checked_add(8)?)?; self.position += 8; Some(f64::from_le_bytes(bytes.try_into().ok()?)) }
    fn varint(&mut self) -> Option<u64> { let mut value = 0; for shift in (0..70).step_by(7) { let byte = self.byte()?; if shift == 63 && byte > 1 { return None; } value |= ((byte & 127) as u64) << shift; if byte < 128 { return Some(value); } } None }
    fn skip(&mut self, count: u64) -> Option<()> { let end = self.position.checked_add(usize::try_from(count).ok()?)?; if end > self.bytes.len() { self.halted = true; self.position = self.bytes.len(); } else { self.position = end; } Some(()) }
    fn string(&mut self) -> Option<String> { let tag = self.byte()?; self.string_body(tag) }
    fn string_body(&mut self, tag: u8) -> Option<String> {
        let length = usize::try_from(self.varint()?).ok()?;
        self.shortened |= length > 256;
        let multiplier = if tag == 8 { 1 } else if tag == 9 { 2 } else { return None; };
        let count = length.checked_mul(multiplier)?;
        let available = self.bytes.len().saturating_sub(self.position).min(count);
        let bytes = &self.bytes[self.position..self.position + available];
        let units: Vec<u16> = if multiplier == 1 { bytes.iter().take(256).map(|byte| *byte as u16).collect() } else { bytes.chunks_exact(2).take(256).map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]])).collect() };
        self.skip(count as u64)?;
        Some(String::from_utf16_lossy(&units))
    }
    fn value(&mut self, depth: usize) -> Option<Preview> {
        if depth > 6 || self.nodes >= 256 { self.halted = true; let mut output = Preview::leaf("Truncated", "Preview budget reached"); output.truncated = true; return Some(output); }
        self.nodes += 1;
        let tag = self.byte()?;
        let object = if (0x11..=0x39).contains(&tag) { let id = self.objects; self.objects += 1; Some(id) } else { None };
        let mut output = match tag {
            1 => Preview::leaf("Undefined", "undefined"), 2 => Preview::leaf("Null", "null"), 3 => Preview::leaf("Boolean", "false"), 4 => Preview::leaf("Boolean", "true"),
            5 => { let value = self.varint()? as u32; Preview::leaf("Number", (((value >> 1) as i32) ^ -((value & 1) as i32)).to_string()) },
            6 | 0x14 | 0x19 => { let value = self.number()?; Preview::leaf(if tag == 0x14 { "Date" } else { "Number" }, value.to_string()) },
            7 | 0x1b => { let length = self.varint()?; let available = self.bytes.len().saturating_sub(self.position).min(length.min(256) as usize); let text = String::from_utf8_lossy(&self.bytes[self.position..self.position+available]).into_owned(); self.skip(length)?; Preview::leaf("BigInt", format!("{text}n")) },
            8 | 9 => { let text = self.string_body(tag)?; Preview::leaf("String", text) },
            0x10 => { let reference = self.varint()?; let mut output = Preview::leaf("Reference", format!("↩ #{reference}")); output.reference = Some(reference); output },
            0x11 | 0x12 => {
                let length = if tag == 0x12 { Some(self.varint()?) } else { None };
                let mut output = Preview::leaf(if tag == 0x12 { "Array" } else { "Object" }, length.map_or_else(|| "Object".into(), |length| format!("Array({length})")));
                output.size = length;
                while !self.halted && self.bytes.get(self.position).copied() != Some(15) {
                    if output.children.len() >= 32 { self.halted = true; break; }
                    let key_tag = self.byte()?;
                    let name = if key_tag == 10 { self.varint()?.to_string() } else { self.string_body(key_tag)? };
                    let value = self.value(depth + 1)?;
                    output.children.push(Child { name, value });
                }
                if !self.halted { self.byte()?; }
                output
            }
            0x15 => { let source = self.string()?; let flags = self.string()?; Preview::leaf("RegExp", format!("/{source}/{flags}")) },
            0x16 | 0x17 | 0x32 => {
                let length = if tag == 0x32 { Some(self.varint()?) } else { None };
                let mut output = Preview::leaf(match tag { 0x16 => "Map", 0x17 => "Set", _ => "FileList" }, "Collection");
                output.size = length;
                while !self.halted && length.map_or_else(|| self.bytes.get(self.position).copied() != Some(15), |length| output.children.len() < length as usize) {
                    if output.children.len() >= 32 { self.halted = true; break; }
                    let name = if tag == 0x16 { format!("{}.{}", output.children.len()/2, if output.children.len()%2 == 0 { "key" } else { "value" }) } else { output.children.len().to_string() };
                    let value = self.value(depth + 1)?;
                    output.children.push(Child { name, value });
                }
                if length.is_none() && !self.halted { self.byte()?; }
                output
            }
            0x18 => Preview::leaf("BooleanObject", (self.byte()? != 0).to_string()),
            0x1a => Preview::leaf("StringObject", self.string()?),
            0x1c => {
                let kind = self.byte()?; let mask = self.byte()?;
                let mut output = Preview::leaf("Error", ["Error", "EvalError", "RangeError", "ReferenceError", "SyntaxError", "TypeError", "URIError"].get(kind as usize).copied().unwrap_or("Error"));
                if mask & 1 != 0 { output.children.push(Child { name: "message".into(), value: Preview::leaf("String", self.string()?) }); }
                if mask & 2 != 0 { output.children.push(Child { name: "stack".into(), value: Preview::leaf("String", self.string()?) }); }
                if mask & 4 != 0 { output.children.push(Child { name: "cause".into(), value: self.value(depth + 1)? }); }
                output
            }
            0x20 | 0x21 => { let length = self.varint()?; if tag == 0x21 { self.varint()?; } self.skip(length)?; let mut output = Preview::leaf("ArrayBuffer", format!("ArrayBuffer({length})")); output.size = Some(length); output },
            0x22 => { let kind = self.byte()?; let offset = self.varint()?; let length = self.varint()?; let buffer = self.value(depth + 1)?; let mut output = Preview::leaf("TypedArray", format!("view kind={kind}, offset={offset}, length={}", length.saturating_sub(1))); output.size = (length != 0).then_some(length.saturating_sub(1)); output.children.push(Child { name: "buffer".into(), value: buffer }); output },
            0x30 | 0x31 => { let mime = self.string()?; let length = self.varint()?; self.skip(length)?; let mut output = Preview::leaf(if tag == 0x31 { "File" } else { "Blob" }, format!("{length} bytes")); output.size = Some(length); output.mime = Some(mime); if tag == 0x31 && !self.halted { output.name = Some(self.string()?); let modified = self.number()?; output.children.push(Child { name: "lastModified".into(), value: Preview::leaf("Number", modified.to_string()) }); self.string()?; } output },
            0x33 => { let width = self.varint()?; let height = self.varint()?; self.byte()?; let mut output = Preview::leaf("ImageData", format!("{width} × {height}")); output.children.push(Child { name: "data".into(), value: self.value(depth + 1)? }); output },
            0x34 | 0x37 => { self.byte()?; let count = if tag == 0x34 { 4 } else if self.byte()? != 0 { 6 } else { 16 }; let mut output = Preview::leaf(if tag == 0x34 { "DOMPoint" } else { "DOMMatrix" }, "Geometry"); for index in 0..count { output.children.push(Child { name: index.to_string(), value: Preview::leaf("Number", self.number()?.to_string()) }); } output },
            0x38 => { self.byte()?; let name = self.string()?; let message = self.string()?; let mask = self.byte()?; if mask & 1 != 0 { self.string()?; } if mask & 2 != 0 { self.number()?; } if mask & 4 != 0 { self.number()?; } Preview::leaf("DOMException", format!("{name}: {message}")) },
            0x39 => { let interface = self.string()?; let size = self.varint()?; self.skip(size)?; let mut output = Preview::leaf("PlatformObject", interface); output.size = Some(size); output },
            _ => return None,
        };
        output.object_id = object;
        output.truncated |= self.halted;
        Some(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cyclic_preview_does_not_follow_references() {
        let bytes = b"RJS\x01\x01\x11\x08\x04self\x10\x00\x0f";
        let result = preview(bytes, bytes.len() as u64);
        assert_eq!(result.children[0].value.reference, Some(0));
        assert_eq!(result.object_id, Some(0));
    }
    #[test]
    fn huge_blob_prefix_reports_metadata_without_materializing_body() {
        let mut bytes = b"RJS\x01\x01\x30\x08\x0atext/plain".to_vec();
        bytes.extend_from_slice(&[0x80, 0x80, 0x40]);
        let result = preview(&bytes, 1_048_600);
        assert_eq!(result.kind, "Blob");
        assert_eq!(result.mime.as_deref(), Some("text/plain"));
        assert_eq!(result.size, Some(1_048_576));
        assert!(result.truncated);
    }
    #[test]
    fn keyset_pages_and_bounded_search_do_not_change_records() {
        use super::super::{backend::{Backend, Durability, StoreMeta}, memory::MemoryBackend};
        let name: Vec<u16> = "inspect-pages".encode_utf16().collect();
        let mut backend = MemoryBackend::new(1 << 20);
        assert!(backend.inspect_existing(&name).is_err());
        assert!(backend.databases().unwrap().is_empty(), "inspection created a missing database");
        let mut database = backend.open(&name).unwrap();
        database.begin_write(Durability::Default).unwrap();
        database.set_version(1).unwrap();
        database.create_store(StoreMeta { id: 1, name: "store".encode_utf16().collect(), key_path: None, auto_increment: false, indexes: vec![] }).unwrap();
        for value in 1..=4 { database.put(1, &key::Key::Number(value as f64).encode(), std::sync::Arc::new(vec![b'R', b'J', b'S', 1, 1, 5, value * 2])).unwrap(); }
        database.commit_write().unwrap();
        let query = |after| Query::Records { database: name.clone(), store: "store".into(), index: None, after, limit: 2, search: String::new() };
        let Response::Records { records, next_cursor, has_more, .. } = read(&mut *database, query(None)).unwrap() else { panic!("wrong response"); };
        assert_eq!(records.len(), 2); assert!(has_more); assert_eq!(records[0].key.preview, "1");
        let Response::Records { records, has_more, .. } = read(&mut *database, query(next_cursor)).unwrap() else { panic!("wrong response"); };
        assert_eq!(records[0].key.preview, "3"); assert_eq!(records.len(), 2); assert!(!has_more);
        assert_eq!(database.count(1, Bound::Unbounded, Bound::Unbounded).unwrap(), 4);
        assert_eq!(database.meta().version, 1);
    }
}
