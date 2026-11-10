//! Canvas text shaping and outlines shared by CPU and host canvas backends.
//!
//! FontFace/@font-face embedders register the original SFNT under its CSS name;
//! platform fonts are discovered once. Layouts and font data are bounded/cached
//! per JS context, and discarded on context teardown or font registration.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};

use fontdb::{Database, Family, Query, Source, Stretch, Style, Weight, ID};
use kurbo::{Affine, BezPath, Rect, Shape};
use rustybuzz::ttf_parser::{GlyphId, OutlineBuilder, Tag};
use rustybuzz::{Direction, Face, Feature, UnicodeBuffer, Variation};
use unicode_bidi::{BidiInfo, Level};

use crate::context::ContextId;

/// An already decoded font file and its CSS FontFace descriptors. Register
/// before renaming its internal family for an embedder's private text system.
pub struct FontRegistration {
    pub family: String,
    pub data: Vec<u8>,
    pub weight: String,
    pub style: String,
    pub stretch: String,
    pub unicode_range: Vec<(u32, u32)>,
}

#[derive(Clone, Debug)]
pub(crate) struct Font {
    pub size: f64,
    families: Vec<String>,
    weight: Weight,
    style: Style,
    stretch: Stretch,
}

/// Canvas has no inherited CSS layout size. Relative sizes use the initial
/// 16px CSS font size; absolute physical units use CSS's 96dpi conversion.
pub(crate) fn length(value: &str, em: f64) -> Option<f64> {
    let value = value.trim();
    for (unit, factor) in [
        ("rem", 16.0),
        ("px", 1.0),
        ("pt", 96.0 / 72.0),
        ("pc", 16.0),
        ("in", 96.0),
        ("cm", 96.0 / 2.54),
        ("mm", 96.0 / 25.4),
        ("em", em),
        ("ex", em * 0.5),
        ("ch", em * 0.5),
        ("%", em / 100.0),
    ] {
        if let Some(number) = value.strip_suffix(unit) {
            return number
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite())
                .map(|v| v * factor);
        }
    }
    (value.parse::<f64>().ok() == Some(0.0)).then_some(0.0)
}

fn weight(value: &str) -> Weight {
    Weight(match value {
        "bold" | "bolder" => 700,
        "lighter" => 100,
        _ => value
            .split_whitespace()
            .next()
            .unwrap_or("")
            .parse::<u16>()
            .unwrap_or(400)
            .clamp(1, 1000),
    })
}

fn style(value: &str) -> Style {
    if value.starts_with("italic") {
        Style::Italic
    } else if value.starts_with("oblique") {
        Style::Oblique
    } else {
        Style::Normal
    }
}

fn stretch(value: &str) -> Stretch {
    match value {
        "ultra-condensed" => Stretch::UltraCondensed,
        "extra-condensed" => Stretch::ExtraCondensed,
        "condensed" => Stretch::Condensed,
        "semi-condensed" => Stretch::SemiCondensed,
        "semi-expanded" => Stretch::SemiExpanded,
        "expanded" => Stretch::Expanded,
        "extra-expanded" => Stretch::ExtraExpanded,
        "ultra-expanded" => Stretch::UltraExpanded,
        _ => Stretch::Normal,
    }
}

fn family_names(value: &str) -> Option<Vec<String>> {
    let mut names = Vec::new();
    let mut name = String::new();
    let mut quote = None;
    let mut escape = false;
    for c in value.chars() {
        if escape {
            name.push(c);
            escape = false;
            continue;
        }
        if c == '\\' {
            escape = true;
            continue;
        }
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else {
                name.push(c);
            }
        } else if c == '\'' || c == '"' {
            quote = Some(c);
        } else if c == ',' {
            let n = name.trim();
            if n.is_empty() {
                return None;
            }
            names.push(n.to_string());
            name.clear();
        } else {
            name.push(c);
        }
    }
    if quote.is_some() || escape || name.trim().is_empty() {
        return None;
    }
    names.push(name.trim().to_string());
    Some(names)
}

impl Font {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        let mut offset = 0;
        let mut font = Self {
            size: 10.0,
            families: Vec::new(),
            weight: Weight::NORMAL,
            style: Style::Normal,
            stretch: Stretch::Normal,
        };
        for token in value.split_whitespace() {
            let start = offset + value[offset..].find(token)?;
            offset = start + token.len();
            let size_token = token.split('/').next()?;
            let size = match size_token {
                "xx-small" => Some(9.0),
                "x-small" => Some(10.0),
                "small" => Some(13.0),
                "medium" => Some(16.0),
                "large" => Some(18.0),
                "x-large" => Some(24.0),
                "xx-large" => Some(32.0),
                "xxx-large" => Some(48.0),
                "smaller" => Some(16.0 / 1.2),
                "larger" => Some(16.0 * 1.2),
                _ => length(size_token, 16.0),
            };
            if let Some(size) = size {
                if size < 0.0 {
                    return None;
                }
                let mut rest = value[offset..].trim();
                if rest.starts_with('/') {
                    rest = rest[1..].trim_start();
                    rest = rest.split_once(char::is_whitespace)?.1.trim_start();
                } else if token.ends_with('/') {
                    rest = rest.split_once(char::is_whitespace)?.1.trim_start();
                }
                font.size = size;
                font.families = family_names(rest)?;
                return Some(font);
            }
            match token {
                "normal" | "small-caps" => {}
                "italic" | "oblique" => font.style = style(token),
                "bold" | "bolder" | "lighter" => font.weight = weight(token),
                "ultra-condensed" | "extra-condensed" | "condensed" | "semi-condensed"
                | "semi-expanded" | "expanded" | "extra-expanded" | "ultra-expanded" => {
                    font.stretch = stretch(token)
                }
                _ if token
                    .parse::<u16>()
                    .ok()
                    .is_some_and(|w| (1..=1000).contains(&w)) =>
                {
                    font.weight = weight(token)
                }
                _ => return None,
            }
        }
        None
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Options<'a> {
    pub font: &'a str,
    pub direction: &'a str,
    pub letter_spacing: &'a str,
    pub word_spacing: &'a str,
    pub kerning: &'a str,
    pub stretch: &'a str,
    pub caps: &'a str,
    pub lang: &'a str,
}

#[derive(Clone)]
pub(crate) struct Layout {
    pub path: BezPath,
    pub width: f64,
    pub ascent: f64,
    pub descent: f64,
    pub rtl: bool,
}

impl Layout {
    pub(crate) fn anchor(&self, align: &str, baseline: &str) -> (f64, f64) {
        let x = match align {
            "center" => -self.width * 0.5,
            "right" => -self.width,
            "start" if self.rtl => -self.width,
            "end" if !self.rtl => -self.width,
            _ => 0.0,
        };
        let y = match baseline {
            "top" => self.ascent,
            "hanging" => self.ascent * 0.8,
            "middle" => (self.ascent - self.descent) * 0.5,
            "ideographic" | "bottom" => -self.descent,
            _ => 0.0,
        };
        (x, y)
    }

    pub(crate) fn bounds(&self) -> Option<Rect> {
        (!self.path.elements().is_empty()).then(|| self.path.bounding_box())
    }

    pub(crate) fn positioned(
        &self,
        x: f64,
        y: f64,
        align: &str,
        baseline: &str,
        max_width: f64,
    ) -> BezPath {
        let (ax, ay) = self.anchor(align, baseline);
        let sx = if self.width > max_width {
            max_width / self.width
        } else {
            1.0
        };
        (Affine::translate((x, y))
            * Affine::scale_non_uniform(sx, 1.0)
            * Affine::translate((ax, ay)))
            * &self.path
    }
}

struct Fonts {
    db: Database,
    system_loaded: bool,
    data: HashMap<ID, (Arc<Vec<u8>>, u32)>,
    ranges: HashMap<ID, Vec<(u32, u32)>>,
    custom: Vec<ID>,
    selected: HashMap<(String, char), Option<ID>>,
    layouts: VecDeque<(String, Arc<Layout>)>,
}

fn system_fonts() -> &'static Database {
    static SYSTEM: OnceLock<Database> = OnceLock::new();
    SYSTEM.get_or_init(|| {
        let mut db = Database::new();
        db.load_system_fonts();
        for (generic, candidates) in [
            (
                "sans",
                &[
                    "Segoe UI",
                    "Arial",
                    "DejaVu Sans",
                    "Noto Sans",
                    "Liberation Sans",
                ][..],
            ),
            (
                "serif",
                &[
                    "Times New Roman",
                    "Times",
                    "DejaVu Serif",
                    "Noto Serif",
                    "Liberation Serif",
                ][..],
            ),
            (
                "mono",
                &[
                    "Consolas",
                    "Menlo",
                    "DejaVu Sans Mono",
                    "Noto Sans Mono",
                    "Liberation Mono",
                ][..],
            ),
        ] {
            if let Some(name) = candidates.iter().find(|name| {
                db.faces()
                    .any(|face| face.families.iter().any(|(family, _)| family == **name))
            }) {
                match generic {
                    "sans" => db.set_sans_serif_family(*name),
                    "serif" => db.set_serif_family(*name),
                    _ => db.set_monospace_family(*name),
                }
            }
        }
        db
    })
}

impl Fonts {
    fn new() -> Self {
        Self {
            db: Database::new(),
            system_loaded: false,
            data: HashMap::new(),
            ranges: HashMap::new(),
            custom: Vec::new(),
            selected: HashMap::new(),
            layouts: VecDeque::new(),
        }
    }

    fn load_system(&mut self) {
        if self.system_loaded {
            return;
        }
        // Keep CSS faces last, so their ranges and descriptors take priority.
        for face in system_fonts().faces() {
            self.db.push_face_info(face.clone());
        }
        for name in ["sans-serif", "serif", "monospace"] {
            let generic = family(name);
            if let Some(id) = system_fonts().query(&Query {
                families: &[generic],
                ..Query::default()
            }) {
                if let Some(face) = system_fonts().face(id) {
                    let name = face.families[0].0.clone();
                    match generic {
                        Family::Serif => self.db.set_serif_family(name),
                        Family::Monospace => self.db.set_monospace_family(name),
                        _ => self.db.set_sans_serif_family(name),
                    }
                }
            }
        }
        self.system_loaded = true;
    }

    fn bytes(&mut self, id: ID) -> Option<(Arc<Vec<u8>>, u32)> {
        if let Some(data) = self.data.get(&id) {
            return Some(data.clone());
        }
        let data = self
            .db
            .with_face_data(id, |bytes, index| (Arc::new(bytes.to_vec()), index))?;
        if self.data.len().saturating_sub(self.custom.len()) >= 16 {
            if let Some(old) = self
                .data
                .keys()
                .copied()
                .find(|old| !self.custom.contains(old))
            {
                self.data.remove(&old);
            }
        }
        self.data.insert(id, data.clone());
        Some(data)
    }

    fn supports(&mut self, id: ID, c: char) -> bool {
        if self.ranges.get(&id).is_some_and(|ranges| {
            !ranges.is_empty() && !ranges.iter().any(|&(a, b)| (a..=b).contains(&(c as u32)))
        }) {
            return false;
        }
        let supports = |data: &[u8], index| {
            let Some(face) = Face::from_slice(data, index) else {
                return false;
            };
            let Some(glyph) = face.glyph_index(c) else {
                return false;
            };
            // Do not retain every system font while searching for one emoji.
            c.is_whitespace()
                || is_invisible(c)
                || face.outline_glyph(glyph, &mut OutlineSink).is_some()
        };
        if let Some((data, index)) = self.data.get(&id) {
            supports(data, *index)
        } else {
            self.db.with_face_data(id, supports).unwrap_or(false)
        }
    }
    fn select(&mut self, font: &Font, c: char, selection: &str) -> Option<ID> {
        if self.selected.len() >= 8192 {
            self.selected.clear();
        }
        let key = (selection.to_string(), c);
        if let Some(id) = self.selected.get(&key) {
            return *id;
        }
        let mut first = None;
        for name in &font.families {
            // CSS subsets may share one family and weight but have different
            // unicode-ranges. Check all matching declarations, newest first.
            let mut ids: Vec<_> = self
                .custom
                .iter()
                .rev()
                .copied()
                .filter(|id| {
                    self.db.face(*id).is_some_and(|f| {
                        f.families.iter().any(|(n, _)| n.eq_ignore_ascii_case(name))
                    })
                })
                .collect();
            ids.sort_by_key(|id| {
                let f = self.db.face(*id).unwrap();
                (
                    u8::from(f.style != font.style),
                    (i32::from(f.weight.0) - i32::from(font.weight.0)).abs(),
                )
            });
            if let Some(id) = self.db.query(&Query {
                families: &[family(name)],
                weight: font.weight,
                style: font.style,
                stretch: font.stretch,
            }) {
                ids.push(id);
            }
            for id in ids {
                first = first.or(Some(id));
                if self.supports(id, c) {
                    self.selected.insert(key, Some(id));
                    return Some(id);
                }
            }
        }
        if !self.system_loaded {
            self.load_system();
            return self.select(font, c, selection);
        }
        let sans = self.db.query(&Query {
            families: &[Family::SansSerif],
            weight: font.weight,
            style: font.style,
            stretch: font.stretch,
        });
        first = first.or(sans);
        if let Some(id) = sans {
            if self.supports(id, c) {
                self.selected.insert(key, Some(id));
                return Some(id);
            }
        }
        let ids: Vec<_> = self.db.faces().map(|f| f.id).collect();
        for id in ids {
            if self.supports(id, c) {
                self.selected.insert(key, Some(id));
                return Some(id);
            }
        }
        // Missing characters still paint the primary font's .notdef outline.
        self.selected.insert(key, first);
        first
    }
}

fn family(name: &str) -> Family<'_> {
    match name.to_ascii_lowercase().as_str() {
        "sans-serif" | "system-ui" | "ui-sans-serif" => Family::SansSerif,
        "serif" | "ui-serif" => Family::Serif,
        "monospace" | "ui-monospace" => Family::Monospace,
        "cursive" => Family::Cursive,
        "fantasy" => Family::Fantasy,
        _ => Family::Name(name),
    }
}

fn registry() -> &'static Mutex<HashMap<ContextId, Fonts>> {
    static REGISTRY: OnceLock<Mutex<HashMap<ContextId, Fonts>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Makes a decoded CSS font available to Canvas2D in one JS context. Invalid
/// files are rejected, without invalidating the last usable layout cache.
pub fn register_font(context: ContextId, font: FontRegistration) -> bool {
    let mut registry = registry().lock().unwrap();
    let fonts = registry.entry(context).or_insert_with(Fonts::new);
    let data = Arc::new(font.data);
    let ids = fonts.db.load_font_source(Source::Binary(data.clone()));
    if ids.is_empty() {
        return false;
    }
    for id in ids {
        let mut face = fonts.db.face(id).unwrap().clone();
        fonts.db.remove_face(id);
        let language = face.families[0].1;
        face.families = vec![(
            font.family.trim().trim_matches(['\'', '"']).to_string(),
            language,
        )];
        face.weight = weight(&font.weight);
        face.style = style(&font.style);
        face.stretch = stretch(&font.stretch);
        let index = face.index;
        let id = fonts.db.push_face_info(face);
        fonts.data.insert(id, (data.clone(), index));
        fonts.ranges.insert(id, font.unicode_range.clone());
        fonts.custom.push(id);
    }
    fonts.selected.clear();
    fonts.layouts.clear();
    true
}

pub(crate) fn forget(context: ContextId) {
    registry().lock().unwrap().remove(&context);
}

fn is_invisible(c: char) -> bool {
    matches!(c, '\u{ad}' | '\u{61c}' | '\u{180e}' | '\u{feff}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{fe00}'..='\u{fe0f}' | '\u{e0100}'..='\u{e01ef}')
}

struct OutlineSink;
impl OutlineBuilder for OutlineSink {
    fn move_to(&mut self, _: f32, _: f32) {}
    fn line_to(&mut self, _: f32, _: f32) {}
    fn quad_to(&mut self, _: f32, _: f32, _: f32, _: f32) {}
    fn curve_to(&mut self, _: f32, _: f32, _: f32, _: f32, _: f32, _: f32) {}
    fn close(&mut self) {}
}

struct Outline<'a> {
    path: &'a mut BezPath,
    x: f64,
    y: f64,
    scale: f64,
}
impl Outline<'_> {
    fn point(&self, x: f32, y: f32) -> (f64, f64) {
        (
            self.x + f64::from(x) * self.scale,
            self.y - f64::from(y) * self.scale,
        )
    }
}
impl OutlineBuilder for Outline<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        self.path.move_to(self.point(x, y));
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.path.line_to(self.point(x, y));
    }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        self.path.quad_to(self.point(x1, y1), self.point(x, y));
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        self.path
            .curve_to(self.point(x1, y1), self.point(x2, y2), self.point(x, y));
    }
    fn close(&mut self) {
        self.path.close_path();
    }
}

pub(crate) fn layout(context: ContextId, value: &str, options: Options<'_>) -> Arc<Layout> {
    let key = format!("{options:?}\0{value}");
    let mut registry = registry().lock().unwrap();
    let fonts = registry.entry(context).or_insert_with(Fonts::new);
    if let Some(index) = fonts.layouts.iter().position(|(k, _)| k == &key) {
        let entry = fonts.layouts.remove(index).unwrap();
        let result = entry.1.clone();
        fonts.layouts.push_back(entry);
        return result;
    }
    let mut font =
        Font::parse(options.font).unwrap_or_else(|| Font::parse("10px sans-serif").unwrap());
    if options.stretch != "normal" {
        font.stretch = stretch(options.stretch);
    }
    let spacing = length(options.letter_spacing, font.size).unwrap_or(0.0);
    let word_spacing = length(options.word_spacing, font.size).unwrap_or(0.0);
    // HTML's text preparation replaces ASCII whitespace, including newlines,
    // with spaces. Callers render separate lines with separate draw calls.
    let text: String = value
        .chars()
        .map(|c| if c.is_ascii_whitespace() { ' ' } else { c })
        .collect();
    let level = match options.direction {
        "rtl" => Level::rtl(),
        _ => Level::ltr(),
    };
    let bidi = BidiInfo::new(&text, Some(level));
    let mut result = Layout {
        path: BezPath::new(),
        width: 0.0,
        ascent: 0.0,
        descent: 0.0,
        rtl: level.is_rtl(),
    };
    let selection = format!(
        "{:?}:{:?}:{:?}:{:?}",
        font.families, font.weight, font.style, font.stretch
    );
    let mut features = Vec::new();
    if options.kerning == "none" {
        features.push(Feature::new(Tag::from_bytes(b"kern"), 0, ..));
    }
    if spacing != 0.0 {
        // Optional ligatures must not swallow letter-spacing boundaries.
        features.push(Feature::new(Tag::from_bytes(b"liga"), 0, ..));
        features.push(Feature::new(Tag::from_bytes(b"clig"), 0, ..));
    }
    for (caps, tags) in [
        ("small-caps", &[b"smcp"][..]),
        ("all-small-caps", &[b"smcp", b"c2sc"][..]),
        ("petite-caps", &[b"pcap"][..]),
        ("all-petite-caps", &[b"pcap", b"c2pc"][..]),
        ("unicase", &[b"unic"][..]),
        ("titling-caps", &[b"titl"][..]),
    ] {
        if options.caps == caps {
            for tag in tags {
                features.push(Feature::new(Tag::from_bytes(tag), 1, ..));
            }
        }
    }
    let mut previous_segment = false;
    for para in &bidi.paragraphs {
        let (levels, visual) = bidi.visual_runs(para, para.range.clone());
        for run in visual {
            let rtl = levels[run.start].is_rtl();
            let mut segments: Vec<(usize, usize, ID)> = Vec::new();
            for (offset, c) in text[run.clone()].char_indices() {
                let Some(id) = fonts.select(&font, c, &selection) else {
                    continue;
                };
                let start = run.start + offset;
                if let Some(last) = segments.last_mut().filter(|s| s.2 == id && s.1 == start) {
                    last.1 = start + c.len_utf8();
                } else {
                    segments.push((start, start + c.len_utf8(), id));
                }
            }
            if rtl {
                segments.reverse();
            }
            for (start, end, id) in segments {
                let Some((data, index)) = fonts.bytes(id) else {
                    continue;
                };
                let Some(mut face) = Face::from_slice(&data, index) else {
                    continue;
                };
                face.set_variations(&[Variation {
                    tag: Tag::from_bytes(b"wght"),
                    value: f32::from(font.weight.0),
                }]);
                let scale = font.size / f64::from(face.units_per_em());
                result.ascent = result.ascent.max(f64::from(face.ascender()) * scale);
                result.descent = result.descent.max(-f64::from(face.descender()) * scale);
                let mut buffer = UnicodeBuffer::new();
                buffer.push_str(&text[start..end]);
                buffer.set_direction(if rtl {
                    Direction::RightToLeft
                } else {
                    Direction::LeftToRight
                });
                if let Ok(lang) = options.lang.parse() {
                    buffer.set_language(lang);
                }
                buffer.guess_segment_properties();
                let glyphs = rustybuzz::shape(&face, &features, buffer);
                if !glyphs.glyph_infos().is_empty() {
                    if previous_segment {
                        result.width += spacing;
                    }
                    previous_segment = true;
                }
                let mut previous_cluster = None;
                for (info, position) in glyphs.glyph_infos().iter().zip(glyphs.glyph_positions()) {
                    if previous_cluster.is_some_and(|cluster| cluster != info.cluster) {
                        result.width += spacing;
                    }
                    previous_cluster = Some(info.cluster);
                    let mut outline = Outline {
                        path: &mut result.path,
                        x: result.width + f64::from(position.x_offset) * scale,
                        y: -f64::from(position.y_offset) * scale,
                        scale,
                    };
                    let character = text[start..end]
                        .get(info.cluster as usize..)
                        .and_then(|s| s.chars().next());
                    // Default-ignorable characters never paint, even from a font
                    // that has a visible glyph for them (macOS's LastResort
                    // covers every code point, and the fallback search finds it),
                    // and take no width, as in a browser.
                    let invisible = character.is_some_and(is_invisible);
                    let drawn = invisible
                        || face
                            .outline_glyph(GlyphId(info.glyph_id as u16), &mut outline)
                            .is_some();
                    if !drawn
                        && position.x_advance != 0
                        && character.is_some_and(|c| !c.is_whitespace() && !is_invisible(c))
                    {
                        // A colour-only emoji font may have a cmap entry without
                        // any outlines. Prefer a real .notdef, then a visible box.
                        if face.outline_glyph(GlyphId(0), &mut outline).is_none() {
                            let left = outline.x;
                            let top = outline.y - font.size * 0.8;
                            let right =
                                left + (f64::from(position.x_advance) * scale).max(font.size * 0.5);
                            let bottom = outline.y;
                            let inset = (font.size / 16.0).max(0.5);
                            outline.path.move_to((left, top));
                            outline.path.line_to((right, top));
                            outline.path.line_to((right, bottom));
                            outline.path.line_to((left, bottom));
                            outline.path.close_path();
                            outline.path.move_to((left + inset, top + inset));
                            outline.path.line_to((left + inset, bottom - inset));
                            outline.path.line_to((right - inset, bottom - inset));
                            outline.path.line_to((right - inset, top + inset));
                            outline.path.close_path();
                        }
                    }
                    if !invisible {
                        result.width += f64::from(position.x_advance) * scale;
                    }
                    let cluster = info.cluster as usize;
                    if text[start..end]
                        .get(cluster..)
                        .is_some_and(|tail| tail.starts_with(' ') || tail.starts_with('\u{a0}'))
                    {
                        result.width += word_spacing;
                    }
                }
            }
        }
    }
    // Empty text still exposes font metrics, but has an empty ink rectangle.
    if result.ascent == 0.0 && result.descent == 0.0 {
        if let Some(id) = fonts.select(&font, ' ', &selection) {
            if let Some((data, index)) = fonts.bytes(id) {
                if let Some(face) = Face::from_slice(&data, index) {
                    let scale = font.size / f64::from(face.units_per_em());
                    result.ascent = f64::from(face.ascender()) * scale;
                    result.descent = -f64::from(face.descender()) * scale;
                }
            }
        }
    }
    let result = Arc::new(result);
    // Avoid retaining megabyte-sized export strings; normal labels remain hot.
    if value.len() <= 4096 && result.path.elements().len() <= 32_768 {
        let mut elements = fonts
            .layouts
            .iter()
            .map(|(_, l)| l.path.elements().len())
            .sum::<usize>();
        while fonts.layouts.len() >= 128 || elements + result.path.elements().len() > 65_536 {
            let Some((_, old)) = fonts.layouts.pop_front() else {
                break;
            };
            elements = elements.saturating_sub(old.path.elements().len());
        }
        fonts.layouts.push_back((key, result.clone()));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn css_font_shorthand_keeps_quoted_family_lists_and_absolute_units() {
        let font =
            Font::parse("italic 700 12pt/1.4 \"A, B\", 'Another Family', sans-serif").unwrap();
        assert_eq!(font.size, 16.0);
        assert_eq!(font.weight, Weight(700));
        assert_eq!(font.style, Style::Italic);
        assert_eq!(font.families, ["A, B", "Another Family", "sans-serif"]);
        assert!(Font::parse("-1px sans-serif").is_none());
        assert!(Font::parse("12px").is_none());
        assert!(Font::parse("bogus 12px Arial").is_none());
        assert!(Font::parse("12px 'unclosed").is_none());
    }

    #[test]
    fn real_font_fixture_has_outlines_for_latin_and_cyrillic() {
        let face = Face::from_slice(
            include_bytes!("../../../testdata/fonts/Roboto-Regular.ttf"),
            0,
        )
        .unwrap();
        for c in ['H', 'g', 'Ж', 'т'] {
            let glyph = face
                .glyph_index(c)
                .expect("fixture covers Latin and Cyrillic");
            assert!(face.outline_glyph(glyph, &mut OutlineSink).is_some());
        }
        assert!(
            face.glyph_hor_advance(face.glyph_index('W').unwrap())
                .unwrap()
                > face
                    .glyph_hor_advance(face.glyph_index('i').unwrap())
                    .unwrap()
        );
    }
    #[test]
    fn css_subset_faces_match_their_declared_unicode_ranges() {
        let context = ContextId(u64::MAX - 1);
        for character in ['A', 'B'] {
            assert!(register_font(
                context,
                FontRegistration {
                    family: "Canvas Subset Fixture".into(),
                    data: include_bytes!("../../../testdata/fonts/Roboto-Regular.ttf").to_vec(),
                    weight: "400".into(),
                    style: "normal".into(),
                    stretch: "normal".into(),
                    unicode_range: vec![(character as u32, character as u32)],
                }
            ));
        }
        {
            let mut registry = registry().lock().unwrap();
            let fonts = registry.get_mut(&context).unwrap();
            let font = Font::parse("20px 'Canvas Subset Fixture'").unwrap();
            assert_eq!(fonts.select(&font, 'A', "fixture"), Some(fonts.custom[0]));
            assert_eq!(fonts.select(&font, 'B', "fixture"), Some(fonts.custom[1]));
        }
        forget(context);
    }
}
