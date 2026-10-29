//! CSS colours as `fillStyle` / `strokeStyle` / `shadowColor` take them, and
//! back as HTML serializes them (`#rrggbb` when opaque, else
//! `rgba(r, g, b, a)`).
//!
//! A canvas keeps 8-bit sRGB, as a browser's does, so every syntax lands
//! there: named colours, `transparent`, hex, `rgb()`/`rgba()` (comma and space
//! forms, numbers and percentages, `/ alpha`), `hsl()`/`hsla()`, `hwb()`,
//! `lab()`/`lch()`, `oklab()`/`oklch()` and `color(srgb …)` — the last ones
//! because a page often feeds `getComputedStyle(el).color` straight in, and a
//! stylesheet written in `oklch()` (Tailwind 4) then hands the canvas one.
//! Wide-gamut colours are clipped to sRGB per channel. `currentcolor` is the
//! caller's to resolve.

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Parsed {
    Rgba([u8; 4]),
    CurrentColor,
}

/// `None` = not a colour: the setter keeps its old value, as HTML says.
pub fn parse(input: &str) -> Option<Parsed> {
    let s = input.trim().to_ascii_lowercase();
    if s == "currentcolor" {
        return Some(Parsed::CurrentColor);
    }
    if s == "transparent" {
        return Some(Parsed::Rgba([0, 0, 0, 0]));
    }
    if let Some(hex) = s.strip_prefix('#') {
        return parse_hex(hex).map(Parsed::Rgba);
    }
    if let Some(open) = s.find('(') {
        if !s.ends_with(')') {
            return None;
        }
        let name = s[..open].trim();
        let body = &s[open + 1..s.len() - 1];
        return parse_fn(name, body).map(Parsed::Rgba);
    }
    named(&s).map(|v| Parsed::Rgba([(v >> 16) as u8, (v >> 8) as u8, v as u8, 255]))
}

/// HTML "serialization of a color": `#rrggbb`, or `rgba(r, g, b, a)` with the
/// shortest alpha that maps back onto the same byte.
pub fn serialize(c: [u8; 4]) -> String {
    if c[3] == 255 {
        return format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2]);
    }
    format!("rgba({}, {}, {}, {})", c[0], c[1], c[2], alpha_text(c[3]))
}

fn alpha_text(a: u8) -> String {
    if a == 0 {
        return "0".into();
    }
    for digits in 1..=3 {
        let scale = 10f64.powi(digits);
        let v = (a as f64 / 255.0 * scale).round() / scale;
        if (v * 255.0).round() as u8 == a {
            let mut t = format!("{v:.*}", digits as usize);
            while t.ends_with('0') {
                t.pop();
            }
            if t.ends_with('.') {
                t.pop();
            }
            return t;
        }
    }
    format!("{:.3}", a as f64 / 255.0)
}

/// Straight sRGB 0..=1, for the command stream.
pub fn to_f32(c: [u8; 4]) -> [f32; 4] {
    [c[0] as f32 / 255.0, c[1] as f32 / 255.0, c[2] as f32 / 255.0, c[3] as f32 / 255.0]
}

fn parse_hex(h: &str) -> Option<[u8; 4]> {
    if !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let d = |i: usize| u8::from_str_radix(&h[i..i + 1], 16).ok().map(|v| v * 17);
    let dd = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
    match h.len() {
        3 => Some([d(0)?, d(1)?, d(2)?, 255]),
        4 => Some([d(0)?, d(1)?, d(2)?, d(3)?]),
        6 => Some([dd(0)?, dd(2)?, dd(4)?, 255]),
        8 => Some([dd(0)?, dd(2)?, dd(4)?, dd(6)?]),
        _ => None,
    }
}

/// One component: a number, a percentage, `none` (0), or an angle.
#[derive(Clone, Copy, Debug)]
enum Tok {
    Num(f64),
    Pct(f64),
    Deg(f64),
    None,
}

fn tok(t: &str) -> Option<Tok> {
    if t == "none" {
        return Some(Tok::None);
    }
    let num = |s: &str| s.parse::<f64>().ok().filter(|v| v.is_finite());
    if let Some(p) = t.strip_suffix('%') {
        return num(p).map(Tok::Pct);
    }
    for (unit, k) in [("deg", 1.0), ("grad", 0.9), ("rad", 180.0 / std::f64::consts::PI), ("turn", 360.0)] {
        if let Some(v) = t.strip_suffix(unit) {
            return num(v).map(|v| Tok::Deg(v * k));
        }
    }
    num(t).map(Tok::Num)
}

/// The components and the alpha (`/ a`, or a 4th comma value). `legacy` = commas.
fn split(body: &str) -> Option<(Vec<Tok>, Option<Tok>, bool)> {
    let legacy = body.contains(',');
    let (main, alpha) = if legacy {
        let parts: Vec<&str> = body.split(',').map(str::trim).collect();
        match parts.len() {
            3 => (parts, None),
            4 => (parts[..3].to_vec(), Some(parts[3])),
            _ => return None,
        }
    } else {
        let (m, a) = match body.split_once('/') {
            Some((m, a)) => (m, Some(a.trim())),
            None => (body, None),
        };
        (m.split_whitespace().collect(), a)
    };
    if main.len() != 3 {
        return None;
    }
    let toks = main.iter().map(|t| tok(t)).collect::<Option<Vec<_>>>()?;
    let alpha = match alpha {
        Some(a) => Some(tok(a)?),
        None => None,
    };
    Some((toks, alpha, legacy))
}

fn alpha_of(a: Option<Tok>) -> Option<f64> {
    Some(match a {
        None => 1.0,
        Some(Tok::Num(v)) => v,
        Some(Tok::Pct(v)) => v / 100.0,
        Some(Tok::None) => 0.0,
        Some(Tok::Deg(_)) => return None,
    }
    .clamp(0.0, 1.0))
}

fn byte(v: f64) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn hue(t: Tok) -> Option<f64> {
    match t {
        Tok::Num(v) | Tok::Deg(v) => Some(v.rem_euclid(360.0)),
        Tok::None => Some(0.0),
        Tok::Pct(_) => None,
    }
}

/// A percentage or a number, as a fraction of `full`.
fn frac(t: Tok, full: f64) -> Option<f64> {
    match t {
        Tok::Pct(v) => Some(v / 100.0),
        Tok::Num(v) => Some(v / full),
        Tok::None => Some(0.0),
        Tok::Deg(_) => None,
    }
}

fn parse_fn(name: &str, body: &str) -> Option<[u8; 4]> {
    if name == "color" {
        return parse_color_fn(body);
    }
    let (t, a, legacy) = split(body)?;
    let a = byte(alpha_of(a)?);
    let rgb = match name {
        "rgb" | "rgba" => {
            if legacy {
                // Legacy syntax: all three numbers or all three percentages.
                let pct = matches!(t[0], Tok::Pct(_));
                if t.iter().any(|x| matches!(x, Tok::Pct(_)) != pct || matches!(x, Tok::None | Tok::Deg(_))) {
                    return None;
                }
            }
            let c = |x: Tok| frac(x, 255.0);
            [c(t[0])?, c(t[1])?, c(t[2])?]
        }
        "hsl" | "hsla" => {
            let h = hue(t[0])?;
            let (s, l) = (frac(t[1], 100.0)?, frac(t[2], 100.0)?);
            hsl_to_rgb(h, s.clamp(0.0, 1.0), l.clamp(0.0, 1.0))
        }
        "hwb" => {
            let h = hue(t[0])?;
            let (w, b) = (frac(t[1], 100.0)?.clamp(0.0, 1.0), frac(t[2], 100.0)?.clamp(0.0, 1.0));
            if w + b >= 1.0 {
                let g = w / (w + b);
                [g, g, g]
            } else {
                let base = hsl_to_rgb(h, 1.0, 0.5);
                base.map(|c| c * (1.0 - w - b) + w)
            }
        }
        "lab" => {
            let l = frac(t[0], 100.0)? * 100.0;
            let ab = |x: Tok| match x {
                Tok::Pct(v) => Some(v / 100.0 * 125.0),
                Tok::Num(v) => Some(v),
                Tok::None => Some(0.0),
                Tok::Deg(_) => None,
            };
            lab_to_srgb(l, ab(t[1])?, ab(t[2])?)
        }
        "lch" => {
            let l = frac(t[0], 100.0)? * 100.0;
            let c = match t[1] {
                Tok::Pct(v) => v / 100.0 * 150.0,
                Tok::Num(v) => v,
                Tok::None => 0.0,
                Tok::Deg(_) => return None,
            };
            let h = hue(t[2])?.to_radians();
            lab_to_srgb(l, c * h.cos(), c * h.sin())
        }
        "oklab" => {
            let l = frac(t[0], 1.0)?;
            let ab = |x: Tok| match x {
                Tok::Pct(v) => Some(v / 100.0 * 0.4),
                Tok::Num(v) => Some(v),
                Tok::None => Some(0.0),
                Tok::Deg(_) => None,
            };
            oklab_to_srgb(l, ab(t[1])?, ab(t[2])?)
        }
        "oklch" => {
            let l = frac(t[0], 1.0)?;
            let c = match t[1] {
                Tok::Pct(v) => v / 100.0 * 0.4,
                Tok::Num(v) => v,
                Tok::None => 0.0,
                Tok::Deg(_) => return None,
            };
            let h = hue(t[2])?.to_radians();
            oklab_to_srgb(l, c * h.cos(), c * h.sin())
        }
        _ => return None,
    };
    Some([byte(rgb[0]), byte(rgb[1]), byte(rgb[2]), a])
}

/// `color(srgb r g b / a)` and `color(srgb-linear …)`.
fn parse_color_fn(body: &str) -> Option<[u8; 4]> {
    let body = body.trim();
    let (space, rest) = body.split_once(char::is_whitespace)?;
    let (t, a, legacy) = split(rest.trim())?;
    if legacy {
        return None;
    }
    let a = byte(alpha_of(a)?);
    let c = |x: Tok| frac(x, 1.0);
    let mut rgb = [c(t[0])?, c(t[1])?, c(t[2])?];
    match space {
        "srgb" => {}
        "srgb-linear" => rgb = rgb.map(linear_to_srgb),
        _ => return None,
    }
    Some([byte(rgb[0]), byte(rgb[1]), byte(rgb[2]), a])
}

fn hsl_to_rgb(h: f64, s: f64, l: f64) -> [f64; 3] {
    let f = |n: f64| {
        let k = (n + h / 30.0) % 12.0;
        let a = s * l.min(1.0 - l);
        l - a * (k - 3.0).min(9.0 - k).clamp(-1.0, 1.0)
    };
    [f(0.0), f(8.0), f(4.0)]
}

fn linear_to_srgb(c: f64) -> f64 {
    let a = c.abs();
    let v = if a <= 0.0031308 { 12.92 * a } else { 1.055 * a.powf(1.0 / 2.4) - 0.055 };
    v.copysign(c)
}

fn linear_rgb_to_srgb(r: f64, g: f64, b: f64) -> [f64; 3] {
    [linear_to_srgb(r), linear_to_srgb(g), linear_to_srgb(b)]
}

/// CIE Lab (D50) → sRGB, through XYZ and the Bradford D50→D65 adaptation.
fn lab_to_srgb(l: f64, a: f64, b: f64) -> [f64; 3] {
    const K: f64 = 24389.0 / 27.0;
    const E: f64 = 216.0 / 24389.0;
    let fy = (l + 16.0) / 116.0;
    let fx = a / 500.0 + fy;
    let fz = fy - b / 200.0;
    let x = if fx.powi(3) > E { fx.powi(3) } else { (116.0 * fx - 16.0) / K };
    let y = if l > K * E { fy.powi(3) } else { l / K };
    let z = if fz.powi(3) > E { fz.powi(3) } else { (116.0 * fz - 16.0) / K };
    let (x, y, z) = (x * 0.3457 / 0.3585, y, z * (1.0 - 0.3457 - 0.3585) / 0.3585);
    // D50 → D65
    let x65 = 0.955473421488075 * x - 0.02309845494876471 * y + 0.06325924320057072 * z;
    let y65 = -0.0283697093338637 * x + 1.0099953980813041 * y + 0.021041441191917323 * z;
    let z65 = 0.012314014864481998 * x - 0.020507649298898964 * y + 1.330365926242124 * z;
    let r = 3.2409699419045226 * x65 - 1.537383177570094 * y65 - 0.4986107602930034 * z65;
    let g = -0.9692436362808796 * x65 + 1.8759675015077202 * y65 + 0.04155505740717559 * z65;
    let bb = 0.05563007969699366 * x65 - 0.20397695888897652 * y65 + 1.0569715142428786 * z65;
    linear_rgb_to_srgb(r, g, bb)
}

/// Oklab → sRGB (Björn Ottosson's matrices).
fn oklab_to_srgb(l: f64, a: f64, b: f64) -> [f64; 3] {
    let l_ = (l + 0.3963377774 * a + 0.2158037573 * b).powi(3);
    let m_ = (l - 0.1055613458 * a - 0.0638541728 * b).powi(3);
    let s_ = (l - 0.0894841775 * a - 1.2914855480 * b).powi(3);
    let r = 4.0767416621 * l_ - 3.3077115913 * m_ + 0.2309699292 * s_;
    let g = -1.2684380046 * l_ + 2.6097574011 * m_ - 0.3413193965 * s_;
    let bb = -0.0041960863 * l_ - 0.7034186147 * m_ + 1.7076147010 * s_;
    linear_rgb_to_srgb(r, g, bb)
}

/// The CSS named colours (CSS Color 4 §6.1).
fn named(s: &str) -> Option<u32> {
    const NAMED: &[(&str, u32)] = &[
        ("aliceblue", 0xf0f8ff), ("antiquewhite", 0xfaebd7), ("aqua", 0x00ffff), ("aquamarine", 0x7fffd4),
        ("azure", 0xf0ffff), ("beige", 0xf5f5dc), ("bisque", 0xffe4c4), ("black", 0x000000),
        ("blanchedalmond", 0xffebcd), ("blue", 0x0000ff), ("blueviolet", 0x8a2be2), ("brown", 0xa52a2a),
        ("burlywood", 0xdeb887), ("cadetblue", 0x5f9ea0), ("chartreuse", 0x7fff00), ("chocolate", 0xd2691e),
        ("coral", 0xff7f50), ("cornflowerblue", 0x6495ed), ("cornsilk", 0xfff8dc), ("crimson", 0xdc143c),
        ("cyan", 0x00ffff), ("darkblue", 0x00008b), ("darkcyan", 0x008b8b), ("darkgoldenrod", 0xb8860b),
        ("darkgray", 0xa9a9a9), ("darkgreen", 0x006400), ("darkgrey", 0xa9a9a9), ("darkkhaki", 0xbdb76b),
        ("darkmagenta", 0x8b008b), ("darkolivegreen", 0x556b2f), ("darkorange", 0xff8c00), ("darkorchid", 0x9932cc),
        ("darkred", 0x8b0000), ("darksalmon", 0xe9967a), ("darkseagreen", 0x8fbc8f), ("darkslateblue", 0x483d8b),
        ("darkslategray", 0x2f4f4f), ("darkslategrey", 0x2f4f4f), ("darkturquoise", 0x00ced1), ("darkviolet", 0x9400d3),
        ("deeppink", 0xff1493), ("deepskyblue", 0x00bfff), ("dimgray", 0x696969), ("dimgrey", 0x696969),
        ("dodgerblue", 0x1e90ff), ("firebrick", 0xb22222), ("floralwhite", 0xfffaf0), ("forestgreen", 0x228b22),
        ("fuchsia", 0xff00ff), ("gainsboro", 0xdcdcdc), ("ghostwhite", 0xf8f8ff), ("gold", 0xffd700),
        ("goldenrod", 0xdaa520), ("gray", 0x808080), ("green", 0x008000), ("greenyellow", 0xadff2f),
        ("grey", 0x808080), ("honeydew", 0xf0fff0), ("hotpink", 0xff69b4), ("indianred", 0xcd5c5c),
        ("indigo", 0x4b0082), ("ivory", 0xfffff0), ("khaki", 0xf0e68c), ("lavender", 0xe6e6fa),
        ("lavenderblush", 0xfff0f5), ("lawngreen", 0x7cfc00), ("lemonchiffon", 0xfffacd), ("lightblue", 0xadd8e6),
        ("lightcoral", 0xf08080), ("lightcyan", 0xe0ffff), ("lightgoldenrodyellow", 0xfafad2), ("lightgray", 0xd3d3d3),
        ("lightgreen", 0x90ee90), ("lightgrey", 0xd3d3d3), ("lightpink", 0xffb6c1), ("lightsalmon", 0xffa07a),
        ("lightseagreen", 0x20b2aa), ("lightskyblue", 0x87cefa), ("lightslategray", 0x778899), ("lightslategrey", 0x778899),
        ("lightsteelblue", 0xb0c4de), ("lightyellow", 0xffffe0), ("lime", 0x00ff00), ("limegreen", 0x32cd32),
        ("linen", 0xfaf0e6), ("magenta", 0xff00ff), ("maroon", 0x800000), ("mediumaquamarine", 0x66cdaa),
        ("mediumblue", 0x0000cd), ("mediumorchid", 0xba55d3), ("mediumpurple", 0x9370db), ("mediumseagreen", 0x3cb371),
        ("mediumslateblue", 0x7b68ee), ("mediumspringgreen", 0x00fa9a), ("mediumturquoise", 0x48d1cc), ("mediumvioletred", 0xc71585),
        ("midnightblue", 0x191970), ("mintcream", 0xf5fffa), ("mistyrose", 0xffe4e1), ("moccasin", 0xffe4b5),
        ("navajowhite", 0xffdead), ("navy", 0x000080), ("oldlace", 0xfdf5e6), ("olive", 0x808000),
        ("olivedrab", 0x6b8e23), ("orange", 0xffa500), ("orangered", 0xff4500), ("orchid", 0xda70d6),
        ("palegoldenrod", 0xeee8aa), ("palegreen", 0x98fb98), ("paleturquoise", 0xafeeee), ("palevioletred", 0xdb7093),
        ("papayawhip", 0xffefd5), ("peachpuff", 0xffdab9), ("peru", 0xcd853f), ("pink", 0xffc0cb),
        ("plum", 0xdda0dd), ("powderblue", 0xb0e0e6), ("purple", 0x800080), ("rebeccapurple", 0x663399),
        ("red", 0xff0000), ("rosybrown", 0xbc8f8f), ("royalblue", 0x4169e1), ("saddlebrown", 0x8b4513),
        ("salmon", 0xfa8072), ("sandybrown", 0xf4a460), ("seagreen", 0x2e8b57), ("seashell", 0xfff5ee),
        ("sienna", 0xa0522d), ("silver", 0xc0c0c0), ("skyblue", 0x87ceeb), ("slateblue", 0x6a5acd),
        ("slategray", 0x708090), ("slategrey", 0x708090), ("snow", 0xfffafa), ("springgreen", 0x00ff7f),
        ("steelblue", 0x4682b4), ("tan", 0xd2b48c), ("teal", 0x008080), ("thistle", 0xd8bfd8),
        ("tomato", 0xff6347), ("turquoise", 0x40e0d0), ("violet", 0xee82ee), ("wheat", 0xf5deb3),
        ("white", 0xffffff), ("whitesmoke", 0xf5f5f5), ("yellow", 0xffff00), ("yellowgreen", 0x9acd32),
    ];
    NAMED.iter().find(|(n, _)| *n == s).map(|(_, v)| *v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgba(s: &str) -> [u8; 4] {
        match parse(s) {
            Some(Parsed::Rgba(c)) => c,
            other => panic!("{s}: {other:?}"),
        }
    }

    #[test]
    fn the_usual_syntaxes_parse() {
        assert_eq!(rgba("red"), [255, 0, 0, 255]);
        assert_eq!(rgba(" RebeccaPurple "), [0x66, 0x33, 0x99, 255]);
        assert_eq!(rgba("#0f08"), [0, 255, 0, 0x88]);
        assert_eq!(rgba("#12345678"), [0x12, 0x34, 0x56, 0x78]);
        assert_eq!(rgba("rgb(255, 0, 0)"), [255, 0, 0, 255]);
        assert_eq!(rgba("rgba(0,0,255,0.5)"), [0, 0, 255, 128]);
        assert_eq!(rgba("rgb(100% 0% 0% / 25%)"), [255, 0, 0, 64]);
        assert_eq!(rgba("rgb(300 -5 0)"), [255, 0, 0, 255]);
        assert_eq!(rgba("hsl(120, 100%, 50%)"), [0, 255, 0, 255]);
        assert_eq!(rgba("hsl(240deg 100% 50% / 0.5)"), [0, 0, 255, 128]);
        assert_eq!(rgba("hwb(0 0% 0%)"), [255, 0, 0, 255]);
        assert_eq!(rgba("transparent"), [0, 0, 0, 0]);
        assert_eq!(parse("currentColor"), Some(Parsed::CurrentColor));
    }

    #[test]
    fn modern_spaces_land_on_srgb() {
        // Pure white and black in every space.
        assert_eq!(rgba("oklch(1 0 0)"), [255, 255, 255, 255]);
        assert_eq!(rgba("oklab(0 0 0)"), [0, 0, 0, 255]);
        assert_eq!(rgba("lab(100 0 0)"), [255, 255, 255, 255]);
        assert_eq!(rgba("lch(0% 0 0)"), [0, 0, 0, 255]);
        assert_eq!(rgba("color(srgb 1 0 0 / 0.5)"), [255, 0, 0, 128]);
        // sRGB red is oklch(0.628 0.2577 29.23).
        let r = rgba("oklch(62.8% 0.2577 29.23deg)");
        assert!(r[0] > 250 && r[1] < 8 && r[2] < 8, "{r:?}");
    }

    #[test]
    fn junk_is_not_a_colour() {
        for s in ["", "redd", "#12", "#ggg", "rgb(1, 2)", "rgb(1 2 3", "rgb(1, 2%, 3)", "url(x)", "rgb(1,2,3,4,5)"] {
            assert_eq!(parse(s), None, "{s}");
        }
    }

    #[test]
    fn serialization_is_html_s() {
        assert_eq!(serialize([255, 0, 0, 255]), "#ff0000");
        assert_eq!(serialize([0, 0, 255, 128]), "rgba(0, 0, 255, 0.5)");
        assert_eq!(serialize([1, 2, 3, 0]), "rgba(1, 2, 3, 0)");
        assert_eq!(serialize([0, 0, 0, 77]), "rgba(0, 0, 0, 0.3)");
        assert_eq!(serialize([0, 0, 0, 1]), "rgba(0, 0, 0, 0.004)");
    }
}
