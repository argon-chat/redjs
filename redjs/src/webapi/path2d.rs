//! Native `Path2D` (HTML Canvas §"Path2D", `CanvasPath` mixin) — path
//! construction and SVG path-data parsing, all of it Rust.
//!
//! ## Representation
//!
//! Everything is normalised on the way in to four primitives — `moveTo`,
//! `lineTo`, a cubic Bézier, and `close`. Arcs, ellipses, quadratics, `rect`
//! and `roundRect` all become cubics. That is not a narrowing: a `Path2D` has
//! **no observable readback in the specification at all** — it is only ever
//! consumed by a canvas or by `addPath` — so the internal form is free, and
//! this one makes `addPath(path, transform)` a pure point transform and gives a
//! renderer a single primitive to consume.
//!
//! ## The one non-standard addition
//!
//! [`to_segments`] is exposed to JS as `Path2D.prototype.toSegments()`. It is
//! **not in the spec**. It exists because without it a path is write-only: no
//! renderer can consume it and no test can observe it, and a test that cannot
//! observe anything is the failure mode this repo has spent a long time
//! learning to design against.
//!
//! ## Deliberate narrowings
//!
//! - Arcs are flattened to cubics at ≤90° per segment (the standard
//!   construction). The maximum radial error is ~2.7e-4 of the radius, which is
//!   below a pixel for any radius under ~3700 px.
//! - SVG path-data errors follow SVG 1.1 §8.3.9 "path data error handling":
//!   parsing stops at the first error and everything before it is kept, rather
//!   than the whole path being discarded.

use core::ffi::c_void;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use super::geometry::{mat_of, Mat};
use super::util::*;
use crate::capi;

// ---------------------------------------------------------------------------
// The path (no JS)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Seg {
    Move(f64, f64),
    Line(f64, f64),
    /// c1x, c1y, c2x, c2y, x, y
    Cubic([f64; 6]),
    Close,
}

#[derive(Clone, Debug, Default)]
pub struct Path {
    pub segs: Vec<Seg>,
    /// Current point; `None` when there is no subpath yet.
    cur: Option<(f64, f64)>,
    /// Start of the current subpath — where `closePath` returns to.
    start: Option<(f64, f64)>,
}

/// Canvas: "If any of the arguments are infinite or NaN, then return" — the
/// method does nothing at all rather than poisoning the path.
#[inline]
fn finite(vs: &[f64]) -> bool {
    vs.iter().all(|v| v.is_finite())
}

impl Path {
    pub fn new() -> Path {
        Path::default()
    }

    pub fn current_point(&self) -> Option<(f64, f64)> {
        self.cur
    }

    pub fn move_to(&mut self, x: f64, y: f64) {
        if !finite(&[x, y]) {
            return;
        }
        self.segs.push(Seg::Move(x, y));
        self.cur = Some((x, y));
        self.start = Some((x, y));
    }

    pub fn line_to(&mut self, x: f64, y: f64) {
        if !finite(&[x, y]) {
            return;
        }
        // Canvas §"lineTo": with no subpath, "ensure there is a subpath for
        // (x, y)" — i.e. it behaves as moveTo, it does not draw from nowhere.
        if self.cur.is_none() {
            self.move_to(x, y);
            return;
        }
        self.segs.push(Seg::Line(x, y));
        self.cur = Some((x, y));
    }

    pub fn cubic_to(&mut self, c1x: f64, c1y: f64, c2x: f64, c2y: f64, x: f64, y: f64) {
        if !finite(&[c1x, c1y, c2x, c2y, x, y]) {
            return;
        }
        if self.cur.is_none() {
            self.move_to(c1x, c1y);
        }
        self.segs.push(Seg::Cubic([c1x, c1y, c2x, c2y, x, y]));
        self.cur = Some((x, y));
    }

    /// Canvas §"quadraticCurveTo" — degree-elevated to a cubic, which is exact.
    pub fn quad_to(&mut self, cx: f64, cy: f64, x: f64, y: f64) {
        if !finite(&[cx, cy, x, y]) {
            return;
        }
        if self.cur.is_none() {
            self.move_to(cx, cy);
        }
        let (px, py) = self.cur.unwrap();
        self.cubic_to(
            px + 2.0 / 3.0 * (cx - px),
            py + 2.0 / 3.0 * (cy - py),
            x + 2.0 / 3.0 * (cx - x),
            y + 2.0 / 3.0 * (cy - y),
            x,
            y,
        );
    }

    pub fn close(&mut self) {
        // Canvas §"closePath": "If the object's path has no subpaths, then
        // return." Then the new subpath starts at the closed one's start.
        if self.cur.is_none() {
            return;
        }
        self.segs.push(Seg::Close);
        self.cur = self.start;
    }

    pub fn rect(&mut self, x: f64, y: f64, w: f64, h: f64) {
        if !finite(&[x, y, w, h]) {
            return;
        }
        self.move_to(x, y);
        self.line_to(x + w, y);
        self.line_to(x + w, y + h);
        self.line_to(x, y + h);
        self.close();
        // Canvas: rect() leaves a fresh subpath at (x, y).
        self.segs.push(Seg::Move(x, y));
        self.cur = Some((x, y));
        self.start = Some((x, y));
    }

    /// Canvas §"ellipse". `Err` carries the spec's IndexSizeError condition.
    pub fn ellipse(
        &mut self,
        x: f64,
        y: f64,
        rx: f64,
        ry: f64,
        rotation: f64,
        start_angle: f64,
        end_angle: f64,
        ccw: bool,
    ) -> Result<(), &'static str> {
        if rx < 0.0 || ry < 0.0 {
            return Err("IndexSizeError: the radii must not be negative");
        }
        if !finite(&[x, y, rx, ry, rotation, start_angle, end_angle]) {
            return Ok(());
        }

        // Canvas: the swept angle is normalised into (0, 2π] going the chosen
        // direction, and a full turn is preserved rather than collapsing to 0 —
        // which is the difference between drawing a circle and drawing nothing.
        let two_pi = std::f64::consts::TAU;
        let mut delta = end_angle - start_angle;
        if ccw {
            if delta > 0.0 {
                delta -= two_pi * (delta / two_pi).ceil().max(1.0);
            }
            if delta <= -two_pi {
                delta = -two_pi;
            }
        } else {
            if delta < 0.0 {
                delta += two_pi * (-delta / two_pi).ceil().max(1.0);
            }
            if delta >= two_pi {
                delta = two_pi;
            }
        }

        let (sin_r, cos_r) = rotation.sin_cos();
        let map = |ux: f64, uy: f64| -> (f64, f64) {
            let (ex, ey) = (ux * rx, uy * ry);
            (x + ex * cos_r - ey * sin_r, y + ex * sin_r + ey * cos_r)
        };
        let start = map(start_angle.cos(), start_angle.sin());
        // Canvas: "ensure there is a subpath" — the arc's first point is joined
        // to the existing subpath with a line, or starts a new one.
        if self.cur.is_none() {
            self.move_to(start.0, start.1);
        } else {
            self.line_to(start.0, start.1);
        }

        // ≤90° per cubic; the classic error bound for this construction.
        let steps = ((delta.abs() / (std::f64::consts::FRAC_PI_2)).ceil() as usize).max(1);
        let step = delta / steps as f64;
        let mut theta = start_angle;
        for _ in 0..steps {
            let next = theta + step;
            let alpha = (step / 2.0).tan();
            let alpha = step.sin() * ((4.0 + 3.0 * alpha * alpha).sqrt() - 1.0) / 3.0;
            let (s1, c1) = theta.sin_cos();
            let (s2, c2) = next.sin_cos();
            let p1 = map(c1, s1);
            let p2 = map(c2, s2);
            let d1 = map(c1 - alpha * s1, s1 + alpha * c1);
            let d2 = map(c2 + alpha * s2, s2 - alpha * c2);
            let _ = p1;
            self.segs.push(Seg::Cubic([d1.0, d1.1, d2.0, d2.1, p2.0, p2.1]));
            self.cur = Some(p2);
            theta = next;
        }
        Ok(())
    }

    pub fn arc(
        &mut self,
        x: f64,
        y: f64,
        r: f64,
        start_angle: f64,
        end_angle: f64,
        ccw: bool,
    ) -> Result<(), &'static str> {
        if r < 0.0 {
            return Err("IndexSizeError: the radius must not be negative");
        }
        self.ellipse(x, y, r, r, 0.0, start_angle, end_angle, ccw)
    }

    /// Canvas §"arcTo": the tangent-circle construction, with the spec's three
    /// degenerate cases collapsing to a straight line to (x1, y1).
    pub fn arc_to(&mut self, x1: f64, y1: f64, x2: f64, y2: f64, r: f64) -> Result<(), &'static str> {
        if r < 0.0 {
            return Err("IndexSizeError: the radius must not be negative");
        }
        if !finite(&[x1, y1, x2, y2, r]) {
            return Ok(());
        }
        if self.cur.is_none() {
            self.move_to(x1, y1);
            return Ok(());
        }
        let (x0, y0) = self.cur.unwrap();

        let (a1x, a1y) = (x0 - x1, y0 - y1);
        let (a2x, a2y) = (x2 - x1, y2 - y1);
        let l1 = (a1x * a1x + a1y * a1y).sqrt();
        let l2 = (a2x * a2x + a2y * a2y).sqrt();
        // Coincident points, zero radius, or collinear → a plain line.
        let cross = a1x * a2y - a1y * a2x;
        if l1 == 0.0 || l2 == 0.0 || r == 0.0 || cross == 0.0 {
            self.line_to(x1, y1);
            return Ok(());
        }
        let (u1x, u1y) = (a1x / l1, a1y / l1);
        let (u2x, u2y) = (a2x / l2, a2y / l2);
        let cos_theta = (u1x * u2x + u1y * u2y).clamp(-1.0, 1.0);
        let theta = cos_theta.acos();
        let tan_half = (theta / 2.0).tan();
        if tan_half == 0.0 {
            self.line_to(x1, y1);
            return Ok(());
        }
        let dist = r / tan_half;
        let t1 = (x1 + u1x * dist, y1 + u1y * dist);
        let t2 = (x1 + u2x * dist, y1 + u2y * dist);
        // Centre lies along the angle bisector at r / sin(θ/2).
        let (bx, by) = (u1x + u2x, u1y + u2y);
        let bl = (bx * bx + by * by).sqrt();
        let cd = r / (theta / 2.0).sin();
        let cx = x1 + bx / bl * cd;
        let cy = y1 + by / bl * cd;
        let sa = (t1.1 - cy).atan2(t1.0 - cx);
        let ea = (t2.1 - cy).atan2(t2.0 - cx);
        // `cross < 0` means turning clockwise in a y-down space.
        self.line_to(t1.0, t1.1);
        self.ellipse(cx, cy, r, r, 0.0, sa, ea, cross > 0.0)
    }

    /// Canvas §"roundRect". `radii` is already resolved to the four corners
    /// (top-left, top-right, bottom-right, bottom-left) as (rx, ry) pairs.
    pub fn round_rect(
        &mut self,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        mut radii: [(f64, f64); 4],
    ) -> Result<(), &'static str> {
        if radii.iter().any(|(a, b)| *a < 0.0 || *b < 0.0) {
            return Err("IndexSizeError: roundRect radii must not be negative");
        }
        if !finite(&[x, y, w, h]) {
            return Ok(());
        }
        // Spec: negative width/height mirror the rectangle AND swap the corner
        // radii with it.
        let (x, w, flip_x) = if w < 0.0 { (x + w, -w, true) } else { (x, w, false) };
        let (y, h, flip_y) = if h < 0.0 { (y + h, -h, true) } else { (y, h, false) };
        if flip_x {
            radii.swap(0, 1);
            radii.swap(2, 3);
        }
        if flip_y {
            radii.swap(0, 3);
            radii.swap(1, 2);
        }

        // Spec: scale all radii down by one common factor so no pair of
        // adjacent radii overflows its edge.
        let mut scale: f64 = 1.0;
        let mut limit = |sum: f64, extent: f64| {
            if sum > 0.0 {
                scale = scale.min(extent / sum);
            }
        };
        limit(radii[0].0 + radii[1].0, w);
        limit(radii[3].0 + radii[2].0, w);
        limit(radii[0].1 + radii[3].1, h);
        limit(radii[1].1 + radii[2].1, h);
        if scale < 1.0 {
            for r in radii.iter_mut() {
                r.0 *= scale;
                r.1 *= scale;
            }
        }

        let [tl, tr, br, bl] = radii;
        self.move_to(x + tl.0, y);
        self.line_to(x + w - tr.0, y);
        self.corner(x + w - tr.0, y + tr.1, tr, 0);
        self.line_to(x + w, y + h - br.1);
        self.corner(x + w - br.0, y + h - br.1, br, 1);
        self.line_to(x + bl.0, y + h);
        self.corner(x + bl.0, y + h - bl.1, bl, 2);
        self.line_to(x, y + tl.1);
        self.corner(x + tl.0, y + tl.1, tl, 3);
        self.close();
        Ok(())
    }

    /// Quarter ellipse at a rounded-rectangle corner. `quadrant` 0..3 runs
    /// top-right, bottom-right, bottom-left, top-left.
    fn corner(&mut self, cx: f64, cy: f64, r: (f64, f64), quadrant: usize) {
        if r.0 == 0.0 && r.1 == 0.0 {
            return;
        }
        let start = [-std::f64::consts::FRAC_PI_2, 0.0, std::f64::consts::FRAC_PI_2, std::f64::consts::PI]
            [quadrant];
        let _ = self.ellipse(cx, cy, r.0, r.1, 0.0, start, start + std::f64::consts::FRAC_PI_2, false);
    }

    /// Canvas §"addPath": append `other`, optionally transformed. The transform
    /// applies to the added points only, and `other` is copied, not aliased.
    pub fn add_path(&mut self, other: &Path, m: Option<&Mat>) {
        let tp = |x: f64, y: f64| -> (f64, f64) {
            match m {
                Some(m) => {
                    let p = m.transform_point([x, y, 0.0, 1.0]);
                    (p[0], p[1])
                }
                None => (x, y),
            }
        };
        for seg in &other.segs {
            match *seg {
                Seg::Move(x, y) => {
                    let (x, y) = tp(x, y);
                    self.segs.push(Seg::Move(x, y));
                    self.cur = Some((x, y));
                    self.start = Some((x, y));
                }
                Seg::Line(x, y) => {
                    let (x, y) = tp(x, y);
                    self.segs.push(Seg::Line(x, y));
                    self.cur = Some((x, y));
                }
                Seg::Cubic(c) => {
                    let a = tp(c[0], c[1]);
                    let b = tp(c[2], c[3]);
                    let e = tp(c[4], c[5]);
                    self.segs.push(Seg::Cubic([a.0, a.1, b.0, b.1, e.0, e.1]));
                    self.cur = Some(e);
                }
                Seg::Close => {
                    self.segs.push(Seg::Close);
                    self.cur = self.start;
                }
            }
        }
    }

    /// The renderer read-out: `[op, args…]` with op 0=move(2), 1=line(2),
    /// 2=cubic(6), 3=close(0). NOT a specified API — see the module docs.
    pub fn to_segments(&self) -> Vec<f64> {
        let mut out = Vec::with_capacity(self.segs.len() * 4);
        for seg in &self.segs {
            match *seg {
                Seg::Move(x, y) => out.extend_from_slice(&[0.0, x, y]),
                Seg::Line(x, y) => out.extend_from_slice(&[1.0, x, y]),
                Seg::Cubic(c) => {
                    out.push(2.0);
                    out.extend_from_slice(&c);
                }
                Seg::Close => out.push(3.0),
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// SVG path data (spec: the `Path2D(path)` constructor parses SVG path data)
// ---------------------------------------------------------------------------

struct Scanner<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Scanner<'a> {
    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\n' | b'\r' | b'\x0c') {
            self.i += 1;
        }
    }
    /// Separator between numbers: whitespace and at most one comma.
    fn sep(&mut self) {
        self.ws();
        if self.i < self.b.len() && self.b[self.i] == b',' {
            self.i += 1;
            self.ws();
        }
    }
    fn number(&mut self) -> Option<f64> {
        self.sep();
        let start = self.i;
        if self.i < self.b.len() && (self.b[self.i] == b'+' || self.b[self.i] == b'-') {
            self.i += 1;
        }
        let mut digits = false;
        while self.i < self.b.len() && self.b[self.i].is_ascii_digit() {
            self.i += 1;
            digits = true;
        }
        if self.i < self.b.len() && self.b[self.i] == b'.' {
            self.i += 1;
            while self.i < self.b.len() && self.b[self.i].is_ascii_digit() {
                self.i += 1;
                digits = true;
            }
        }
        if !digits {
            self.i = start;
            return None;
        }
        if self.i < self.b.len() && (self.b[self.i] | 0x20) == b'e' {
            let save = self.i;
            self.i += 1;
            if self.i < self.b.len() && (self.b[self.i] == b'+' || self.b[self.i] == b'-') {
                self.i += 1;
            }
            let mut edig = false;
            while self.i < self.b.len() && self.b[self.i].is_ascii_digit() {
                self.i += 1;
                edig = true;
            }
            if !edig {
                self.i = save;
            }
        }
        std::str::from_utf8(&self.b[start..self.i]).ok()?.parse::<f64>().ok()
    }
    /// The arc flag grammar is a single `0` or `1` — NOT a number, so `a1 1 0
    /// 1150` parses as three flags and a coordinate (SVG 1.1 §8.3.8).
    fn flag(&mut self) -> Option<bool> {
        self.sep();
        match self.b.get(self.i) {
            Some(b'0') => {
                self.i += 1;
                Some(false)
            }
            Some(b'1') => {
                self.i += 1;
                Some(true)
            }
            _ => None,
        }
    }
    fn command(&mut self) -> Option<u8> {
        self.ws();
        let c = *self.b.get(self.i)?;
        if c.is_ascii_alphabetic() {
            self.i += 1;
            Some(c)
        } else {
            None
        }
    }
    fn at_end(&mut self) -> bool {
        self.ws();
        self.i >= self.b.len()
    }
}

/// Parse SVG path data into a [`Path`].
///
/// Error handling is SVG 1.1 §8.3.9: on the first malformed token parsing
/// STOPS and everything already parsed is kept. That is why this returns a path
/// and not a `Result` — a partial path is the specified outcome, not a failure.
pub fn parse_svg_path(d: &str) -> Path {
    let mut p = Path::new();
    let mut sc = Scanner { b: d.as_bytes(), i: 0 };
    let mut cmd = 0u8;
    // Previous control point, for the S/T smooth reflections.
    let mut last_cubic_ctrl: Option<(f64, f64)> = None;
    let mut last_quad_ctrl: Option<(f64, f64)> = None;

    loop {
        if sc.at_end() {
            break;
        }
        match sc.command() {
            Some(c) => cmd = c,
            None => {
                // An implicit repeat is only legal after a command that has one.
                if cmd == 0 || cmd == b'Z' || cmd == b'z' {
                    break;
                }
                // A moveto's implicit repeats are linetos (SVG 1.1 §8.3.2).
                if cmd == b'M' {
                    cmd = b'L';
                } else if cmd == b'm' {
                    cmd = b'l';
                }
            }
        }
        let rel = cmd.is_ascii_lowercase();
        let (cx, cy) = p.current_point().unwrap_or((0.0, 0.0));
        let ox = if rel { cx } else { 0.0 };
        let oy = if rel { cy } else { 0.0 };

        macro_rules! num {
            () => {
                match sc.number() {
                    Some(v) => v,
                    None => return p,
                }
            };
        }

        match cmd.to_ascii_uppercase() {
            b'M' => {
                let (x, y) = (num!() + ox, num!() + oy);
                p.move_to(x, y);
                last_cubic_ctrl = None;
                last_quad_ctrl = None;
            }
            b'L' => {
                let (x, y) = (num!() + ox, num!() + oy);
                p.line_to(x, y);
                last_cubic_ctrl = None;
                last_quad_ctrl = None;
            }
            b'H' => {
                let x = num!() + ox;
                p.line_to(x, cy);
                last_cubic_ctrl = None;
                last_quad_ctrl = None;
            }
            b'V' => {
                let y = num!() + oy;
                p.line_to(cx, y);
                last_cubic_ctrl = None;
                last_quad_ctrl = None;
            }
            b'C' => {
                let (c1x, c1y) = (num!() + ox, num!() + oy);
                let (c2x, c2y) = (num!() + ox, num!() + oy);
                let (x, y) = (num!() + ox, num!() + oy);
                p.cubic_to(c1x, c1y, c2x, c2y, x, y);
                last_cubic_ctrl = Some((c2x, c2y));
                last_quad_ctrl = None;
            }
            b'S' => {
                // Reflection of the previous cubic's second control point about
                // the current point; the current point itself when there was no
                // previous cubic (SVG 1.1 §8.3.6).
                let (rx, ry) = match last_cubic_ctrl {
                    Some((px, py)) => (2.0 * cx - px, 2.0 * cy - py),
                    None => (cx, cy),
                };
                let (c2x, c2y) = (num!() + ox, num!() + oy);
                let (x, y) = (num!() + ox, num!() + oy);
                p.cubic_to(rx, ry, c2x, c2y, x, y);
                last_cubic_ctrl = Some((c2x, c2y));
                last_quad_ctrl = None;
            }
            b'Q' => {
                let (qx, qy) = (num!() + ox, num!() + oy);
                let (x, y) = (num!() + ox, num!() + oy);
                p.quad_to(qx, qy, x, y);
                last_quad_ctrl = Some((qx, qy));
                last_cubic_ctrl = None;
            }
            b'T' => {
                let (qx, qy) = match last_quad_ctrl {
                    Some((px, py)) => (2.0 * cx - px, 2.0 * cy - py),
                    None => (cx, cy),
                };
                let (x, y) = (num!() + ox, num!() + oy);
                p.quad_to(qx, qy, x, y);
                last_quad_ctrl = Some((qx, qy));
                last_cubic_ctrl = None;
            }
            b'A' => {
                let (rx, ry) = (num!(), num!());
                let rot = num!();
                let large = match sc.flag() {
                    Some(f) => f,
                    None => return p,
                };
                let sweep = match sc.flag() {
                    Some(f) => f,
                    None => return p,
                };
                let (x, y) = (num!() + ox, num!() + oy);
                svg_arc(&mut p, cx, cy, rx, ry, rot, large, sweep, x, y);
                last_cubic_ctrl = None;
                last_quad_ctrl = None;
            }
            b'Z' => {
                p.close();
                last_cubic_ctrl = None;
                last_quad_ctrl = None;
            }
            _ => return p, // unknown command: stop, keep what we have
        }
    }
    p
}

/// SVG endpoint-parameterised elliptical arc → centre parameterisation → cubics
/// (SVG 1.1 implementation notes F.6.5 and F.6.6).
#[allow(clippy::too_many_arguments)]
fn svg_arc(
    p: &mut Path,
    x1: f64,
    y1: f64,
    rx: f64,
    ry: f64,
    rot_deg: f64,
    large: bool,
    sweep: bool,
    x2: f64,
    y2: f64,
) {
    // F.6.2: zero radii, or coincident endpoints, degenerate to a line.
    let (mut rx, mut ry) = (rx.abs(), ry.abs());
    if rx == 0.0 || ry == 0.0 || (x1 == x2 && y1 == y2) {
        p.line_to(x2, y2);
        return;
    }
    let phi = rot_deg.to_radians();
    let (sin_p, cos_p) = phi.sin_cos();
    let dx2 = (x1 - x2) / 2.0;
    let dy2 = (y1 - y2) / 2.0;
    let x1p = cos_p * dx2 + sin_p * dy2;
    let y1p = -sin_p * dx2 + cos_p * dy2;

    // F.6.6: scale the radii up if they are too small to span the endpoints.
    let lambda = (x1p * x1p) / (rx * rx) + (y1p * y1p) / (ry * ry);
    if lambda > 1.0 {
        let s = lambda.sqrt();
        rx *= s;
        ry *= s;
    }

    let num = (rx * rx * ry * ry - rx * rx * y1p * y1p - ry * ry * x1p * x1p).max(0.0);
    let den = rx * rx * y1p * y1p + ry * ry * x1p * x1p;
    let coef = if den == 0.0 { 0.0 } else { (num / den).sqrt() } * if large == sweep { -1.0 } else { 1.0 };
    let cxp = coef * rx * y1p / ry;
    let cyp = -coef * ry * x1p / rx;
    let cx = cos_p * cxp - sin_p * cyp + (x1 + x2) / 2.0;
    let cy = sin_p * cxp + cos_p * cyp + (y1 + y2) / 2.0;

    let ang = |ux: f64, uy: f64| uy.atan2(ux);
    let theta1 = ang((x1p - cxp) / rx, (y1p - cyp) / ry);
    let theta2 = ang((-x1p - cxp) / rx, (-y1p - cyp) / ry);
    let mut dtheta = theta2 - theta1;
    let two_pi = std::f64::consts::TAU;
    if !sweep && dtheta > 0.0 {
        dtheta -= two_pi;
    } else if sweep && dtheta < 0.0 {
        dtheta += two_pi;
    }

    // `ellipse` re-derives the sweep from start/end and the direction flag, and
    // it also emits the leading `lineTo` — which for an arc segment is a no-op
    // because the start point IS the current point.
    let _ = p.ellipse(cx, cy, rx, ry, phi, theta1, theta1 + dtheta, dtheta < 0.0);
}

// ---------------------------------------------------------------------------
// The JS class
// ---------------------------------------------------------------------------

pub struct PathData {
    pub p: Path,
}

struct PathClasses {
    cls: capi::JSClassRef,
    class: Class,
}
unsafe impl Send for PathClasses {}
unsafe impl Sync for PathClasses {}

fn registry() -> &'static Mutex<HashMap<usize, &'static PathClasses>> {
    static R: OnceLock<Mutex<HashMap<usize, &'static PathClasses>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn classes(g: *mut c_void) -> &'static PathClasses {
    registry().lock().unwrap().get(&(g as usize)).copied().expect("webapi::path2d not installed")
}

pub(crate) fn forget(gone: crate::context::Gone) {
    registry().lock().unwrap().remove(&gone.key);
}

unsafe fn this_path<'a>(g: *mut c_void, cf: *mut c_void) -> Option<&'a mut PathData> {
    host_data::<PathData>(g, classes(g).cls, cf_this(cf))
}

/// Raw path of a `Path2D` value — for a canvas implementation.
pub unsafe fn path_of<'a>(g: *mut c_void, v: u64) -> Option<&'a mut PathData> {
    host_data::<PathData>(g, classes(g).cls, v)
}

#[crate::host_fn]
unsafe fn construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let c = classes(g);
    let arg = cf_arg(cf, 0);
    let p = if arg == UNDEF || arg == NULL {
        Path::new()
    } else if let Some(other) = host_data::<PathData>(g, c.cls, arg) {
        other.p.clone()
    } else {
        // Spec: any other value is stringified and parsed as SVG path data.
        parse_svg_path(&read_string(g, arg).unwrap_or_default())
    };
    let proto = {
        let nt = cf_new_target(cf);
        let pr = if is_cell(nt) { get(g, nt, "prototype") } else { UNDEF };
        if is_cell(pr) { pr } else { c.class.proto }
    };
    host_create(g, c.cls, proto, PathData { p })
}

/// Body shared by every path method: brand-check, run, and map a spec error
/// onto a thrown TypeError naming the spec's DOMException.
macro_rules! path_method {
    ($name:ident, $label:literal, |$p:ident, $g:ident, $cf:ident| $body:expr) => {
        #[crate::host_fn]
        unsafe fn $name($g: *mut c_void, $cf: *mut c_void) -> u64 {
            let Some(data) = this_path($g, $cf) else {
                return throw_type_error($g, concat!("Path2D.prototype.", $label, " called on a non-Path2D"));
            };
            let $p = &mut data.p;
            let r: Result<(), &'static str> = $body;
            match r {
                Ok(()) => UNDEF,
                Err(e) => throw_geometry_error($g, e),
            }
        }
    };
}

path_method!(m_move_to, "moveTo", |p, g, cf| {
    p.move_to(arg_num(g, cf, 0, f64::NAN), arg_num(g, cf, 1, f64::NAN));
    Ok(())
});
path_method!(m_line_to, "lineTo", |p, g, cf| {
    p.line_to(arg_num(g, cf, 0, f64::NAN), arg_num(g, cf, 1, f64::NAN));
    Ok(())
});
path_method!(m_close, "closePath", |p, _g, _cf| {
    p.close();
    Ok(())
});
path_method!(m_bezier, "bezierCurveTo", |p, g, cf| {
    p.cubic_to(
        arg_num(g, cf, 0, f64::NAN), arg_num(g, cf, 1, f64::NAN),
        arg_num(g, cf, 2, f64::NAN), arg_num(g, cf, 3, f64::NAN),
        arg_num(g, cf, 4, f64::NAN), arg_num(g, cf, 5, f64::NAN),
    );
    Ok(())
});
path_method!(m_quad, "quadraticCurveTo", |p, g, cf| {
    p.quad_to(
        arg_num(g, cf, 0, f64::NAN), arg_num(g, cf, 1, f64::NAN),
        arg_num(g, cf, 2, f64::NAN), arg_num(g, cf, 3, f64::NAN),
    );
    Ok(())
});
path_method!(m_rect, "rect", |p, g, cf| {
    p.rect(
        arg_num(g, cf, 0, f64::NAN), arg_num(g, cf, 1, f64::NAN),
        arg_num(g, cf, 2, f64::NAN), arg_num(g, cf, 3, f64::NAN),
    );
    Ok(())
});
path_method!(m_arc, "arc", |p, g, cf| p.arc(
    arg_num(g, cf, 0, f64::NAN), arg_num(g, cf, 1, f64::NAN), arg_num(g, cf, 2, f64::NAN),
    arg_num(g, cf, 3, f64::NAN), arg_num(g, cf, 4, f64::NAN),
    truthy_flag(cf_arg(cf, 5)),
));
path_method!(m_arc_to, "arcTo", |p, g, cf| p.arc_to(
    arg_num(g, cf, 0, f64::NAN), arg_num(g, cf, 1, f64::NAN),
    arg_num(g, cf, 2, f64::NAN), arg_num(g, cf, 3, f64::NAN), arg_num(g, cf, 4, f64::NAN),
));
path_method!(m_ellipse, "ellipse", |p, g, cf| p.ellipse(
    arg_num(g, cf, 0, f64::NAN), arg_num(g, cf, 1, f64::NAN),
    arg_num(g, cf, 2, f64::NAN), arg_num(g, cf, 3, f64::NAN), arg_num(g, cf, 4, f64::NAN),
    arg_num(g, cf, 5, f64::NAN), arg_num(g, cf, 6, f64::NAN),
    truthy_flag(cf_arg(cf, 7)),
));

/// Spec §"roundRect": `radii` is a number, a `{x,y}`-ish, or a list of 1..4 of
/// either, expanded by the CSS `border-radius` shorthand rules.
pub(crate) unsafe fn read_radii(g: *mut c_void, v: u64) -> Result<[(f64, f64); 4], String> {
    unsafe fn one(g: *mut c_void, v: u64) -> (f64, f64) {
        if is_cell(v) && get(g, v, "x") != UNDEF {
            let rx = read_number(g, get(g, v, "x"));
            let ry_v = get(g, v, "y");
            let ry = if ry_v == UNDEF { rx } else { read_number(g, ry_v) };
            return (rx, ry);
        }
        let n = read_number(g, v);
        (n, n)
    }
    if v == UNDEF {
        return Ok([(0.0, 0.0); 4]);
    }
    if is_cell(v) && get(g, v, "length") != UNDEF {
        let n = array_len(g, v);
        let vals: Vec<(f64, f64)> = (0..n).map(|i| one(g, get_i(g, v, i))).collect();
        return match vals.len() {
            1 => Ok([vals[0]; 4]),
            2 => Ok([vals[0], vals[1], vals[0], vals[1]]),
            3 => Ok([vals[0], vals[1], vals[2], vals[1]]),
            4 => Ok([vals[0], vals[1], vals[2], vals[3]]),
            other => Err(format!("roundRect: radii must have 1 to 4 entries, got {other}")),
        };
    }
    Ok([one(g, v); 4])
}

/// The geometry layer's refusals are spelled `"IndexSizeError: why"` — the
/// spec's "throw an IndexSizeError DOMException" — and become exactly that.
pub(crate) unsafe fn throw_geometry_error(g: *mut c_void, e: &str) -> u64 {
    match e.strip_prefix("IndexSizeError: ") {
        Some(why) => super::domexception::throw(g, "IndexSizeError", why),
        None => throw_type_error(g, e),
    }
}

#[crate::host_fn]
unsafe fn m_round_rect(g: *mut c_void, cf: *mut c_void) -> u64 {
    let radii = match read_radii(g, cf_arg(cf, 4)) {
        Ok(r) => r,
        // Spec: a radii list of size other than 1–4 is a RangeError.
        Err(e) => return throw_error(g, 2, &e),
    };
    let (x, y, w, h) = (
        arg_num(g, cf, 0, f64::NAN),
        arg_num(g, cf, 1, f64::NAN),
        arg_num(g, cf, 2, f64::NAN),
        arg_num(g, cf, 3, f64::NAN),
    );
    let Some(data) = this_path(g, cf) else {
        return throw_type_error(g, "Path2D.prototype.roundRect called on a non-Path2D");
    };
    match data.p.round_rect(x, y, w, h, radii) {
        Ok(()) => UNDEF,
        Err(e) => throw_geometry_error(g, e),
    }
}

#[crate::host_fn]
unsafe fn m_add_path(g: *mut c_void, cf: *mut c_void) -> u64 {
    let c = classes(g);
    let other = match host_data::<PathData>(g, c.cls, cf_arg(cf, 0)) {
        Some(o) => o.p.clone(),
        None => return throw_type_error(g, "Path2D.prototype.addPath: argument is not a Path2D"),
    };
    let m = mat_of(g, cf_arg(cf, 1));
    let Some(data) = this_path(g, cf) else {
        return throw_type_error(g, "Path2D.prototype.addPath called on a non-Path2D");
    };
    data.p.add_path(&other, m.as_ref());
    UNDEF
}

#[crate::host_fn]
unsafe fn m_to_segments(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(data) = this_path(g, cf) else {
        return throw_type_error(g, "Path2D.prototype.toSegments called on a non-Path2D");
    };
    let vals = data.p.to_segments();
    let mut out = UNDEF;
    if crate::ffi::redjs_typedarray_create(g, 9 /* f64 */, vals.len(), &mut out) != 0 {
        return UNDEF;
    }
    if !vals.is_empty() {
        let mut ta = crate::ffi::RjsTaInfo::default();
        if crate::glue::rjs_typedarray_info(g, out, &mut ta) == 0 && !ta.data.is_null() {
            core::ptr::copy_nonoverlapping(vals.as_ptr(), ta.data as *mut f64, vals.len());
        }
    }
    out
}

pub unsafe fn install(g: *mut c_void) {
    let cls = make_host_class::<PathData>("Path2D");
    let class = define_class(g, &ClassSpec {
        name: "Path2D",
        arity: 1,
        construct: Some(construct),
        parent_proto: 0,
        methods: &[
            ("moveTo", 2, m_move_to),
            ("lineTo", 2, m_line_to),
            ("closePath", 0, m_close),
            ("bezierCurveTo", 6, m_bezier),
            ("quadraticCurveTo", 4, m_quad),
            ("arc", 6, m_arc),
            ("arcTo", 5, m_arc_to),
            ("ellipse", 8, m_ellipse),
            ("rect", 4, m_rect),
            ("roundRect", 5, m_round_rect),
            ("addPath", 2, m_add_path),
            // NOT in the spec — the renderer read-out. See the module docs.
            ("toSegments", 0, m_to_segments),
        ],
        getters: &[],
        accessors: &[],
    });
    let boxed: &'static PathClasses = Box::leak(Box::new(PathClasses { cls, class }));
    registry().lock().unwrap().insert(g as usize, boxed);
}

// ---------------------------------------------------------------------------
// Tests — the geometry and the parser, no JS context needed
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    /// The end point of the path, walking the segment list.
    fn end_of(p: &Path) -> (f64, f64) {
        p.current_point().expect("path has no current point")
    }

    #[test]
    fn line_to_on_an_empty_path_is_a_move_not_a_line_from_the_origin() {
        // Canvas §lineTo: "ensure there is a subpath". Getting this wrong draws
        // a stray line from (0,0) — visible, but easy to blame on the caller.
        let mut p = Path::new();
        p.line_to(10.0, 10.0);
        assert_eq!(p.segs, vec![Seg::Move(10.0, 10.0)]);
    }

    #[test]
    fn close_path_returns_the_current_point_to_the_subpath_start() {
        let mut p = Path::new();
        p.move_to(5.0, 5.0);
        p.line_to(20.0, 5.0);
        p.close();
        assert_eq!(end_of(&p), (5.0, 5.0));
        // …and a closePath with no subpath at all does nothing.
        let mut q = Path::new();
        q.close();
        assert!(q.segs.is_empty());
    }

    #[test]
    fn non_finite_arguments_leave_the_path_untouched() {
        let mut p = Path::new();
        p.move_to(1.0, 1.0);
        let before = p.segs.len();
        p.line_to(f64::NAN, 5.0);
        p.line_to(f64::INFINITY, 5.0);
        p.cubic_to(0.0, 0.0, 0.0, f64::NAN, 1.0, 1.0);
        assert_eq!(p.segs.len(), before);
        assert_eq!(end_of(&p), (1.0, 1.0));
    }

    #[test]
    fn a_negative_radius_is_an_index_size_error() {
        let mut p = Path::new();
        assert!(p.arc(0.0, 0.0, -1.0, 0.0, 1.0, false).is_err());
        assert!(p.ellipse(0.0, 0.0, 1.0, -1.0, 0.0, 0.0, 1.0, false).is_err());
        assert!(p.arc_to(1.0, 1.0, 2.0, 2.0, -1.0).is_err());
    }

    #[test]
    fn a_full_circle_is_not_flattened_to_nothing() {
        // start == end with a 2π sweep must draw a whole circle, not an empty
        // arc. The normalisation that gets this wrong silently draws nothing.
        let mut p = Path::new();
        p.arc(0.0, 0.0, 10.0, 0.0, std::f64::consts::TAU, false).unwrap();
        let cubics = p.segs.iter().filter(|s| matches!(s, Seg::Cubic(_))).count();
        assert_eq!(cubics, 4, "a full turn should be four ≤90° cubics: {:?}", p.segs);
        assert!(close(end_of(&p).0, 10.0) && close(end_of(&p).1, 0.0));
    }

    #[test]
    fn an_arc_stays_on_its_circle() {
        let mut p = Path::new();
        p.arc(3.0, -4.0, 7.0, 0.2, 5.0, false).unwrap();
        for seg in &p.segs {
            if let Seg::Cubic(c) = seg {
                let (x, y) = (c[4] - 3.0, c[5] + 4.0);
                assert!(close((x * x + y * y).sqrt(), 7.0), "endpoint off the circle: {seg:?}");
            }
        }
    }

    #[test]
    fn quadratic_curves_elevate_exactly() {
        // Degree elevation is exact, so the midpoint of the cubic must equal the
        // midpoint of the quadratic.
        let mut p = Path::new();
        p.move_to(0.0, 0.0);
        p.quad_to(10.0, 20.0, 20.0, 0.0);
        let Seg::Cubic(c) = p.segs[1] else { panic!("expected a cubic") };
        let mid_cubic = |i: usize| 0.125 * (if i == 0 { 0.0 } else { 0.0 }) + 0.375 * c[i] + 0.375 * c[2 + i] + 0.125 * c[4 + i];
        // B(0.5) for the quadratic (0,0)-(10,20)-(20,0) is (10, 10).
        assert!(close(mid_cubic(0), 10.0), "{}", mid_cubic(0));
        assert!(close(mid_cubic(1), 10.0), "{}", mid_cubic(1));
    }

    #[test]
    fn svg_relative_and_absolute_commands_agree() {
        let a = parse_svg_path("M10 10 L20 30 L40 30");
        let b = parse_svg_path("m10 10 l10 20 l20 0");
        assert_eq!(a.segs, b.segs);
    }

    #[test]
    fn an_implicit_repeat_after_moveto_is_a_lineto() {
        // SVG 1.1 §8.3.2. Reading it as another moveto silently breaks every
        // polygon written in the common shorthand.
        let p = parse_svg_path("M0 0 10 0 10 10");
        assert_eq!(p.segs, vec![Seg::Move(0., 0.), Seg::Line(10., 0.), Seg::Line(10., 10.)]);
    }

    #[test]
    fn the_smooth_cubic_reflects_the_previous_control_point() {
        let p = parse_svg_path("M0 0 C 10 10 20 10 30 0 S 40 -10 50 0");
        let Seg::Cubic(c) = p.segs[2] else { panic!("expected the S segment as a cubic") };
        // Reflection of (20,10) about (30,0) is (40,-10).
        assert!(close(c[0], 40.0) && close(c[1], -10.0), "{c:?}");
    }

    #[test]
    fn a_smooth_cubic_with_no_predecessor_uses_the_current_point() {
        let p = parse_svg_path("M5 5 S 10 10 20 20");
        let Seg::Cubic(c) = p.segs[1] else { panic!("expected a cubic") };
        assert!(close(c[0], 5.0) && close(c[1], 5.0), "{c:?}");
    }

    #[test]
    fn arc_flags_are_single_characters_not_numbers() {
        // `a1 1 0 1150` is rx=1 ry=1 rot=0 large=1 sweep=1 x=50 (SVG 1.1
        // §8.3.8). Parsing the flags as numbers eats "1150" and loses the arc.
        let p = parse_svg_path("M0 0 a1 1 0 1150 0");
        assert!(p.segs.len() > 1, "the arc was dropped: {:?}", p.segs);
        assert!(close(end_of(&p).0, 50.0), "end x was {}", end_of(&p).0);
    }

    #[test]
    fn an_svg_arc_reaches_its_endpoint() {
        let p = parse_svg_path("M100 100 A 50 30 20 1 0 200 150");
        assert!(close(end_of(&p).0, 200.0) && close(end_of(&p).1, 150.0), "{:?}", end_of(&p));
    }

    #[test]
    fn an_svg_arc_with_a_zero_radius_becomes_a_line() {
        // SVG F.6.2. A silent no-op here loses the segment entirely.
        let p = parse_svg_path("M0 0 A 0 30 0 0 1 40 0");
        assert_eq!(p.segs, vec![Seg::Move(0., 0.), Seg::Line(40., 0.)]);
    }

    #[test]
    fn malformed_path_data_keeps_what_parsed_and_stops() {
        // SVG 1.1 §8.3.9. Returning an EMPTY path here would silently erase a
        // drawing over one stray character.
        let p = parse_svg_path("M0 0 L10 10 L oops 30 40");
        assert_eq!(p.segs, vec![Seg::Move(0., 0.), Seg::Line(10., 10.)]);
    }

    #[test]
    fn scientific_notation_and_sign_run_together_parse() {
        let p = parse_svg_path("M1e2 1E-2 L-3.5-4.5");
        assert_eq!(p.segs[0], Seg::Move(100.0, 0.01));
        assert_eq!(p.segs[1], Seg::Line(-3.5, -4.5));
    }

    #[test]
    fn add_path_transforms_the_added_points_only() {
        let mut base = Path::new();
        base.move_to(0.0, 0.0);
        let mut other = Path::new();
        other.move_to(1.0, 0.0);
        other.line_to(2.0, 0.0);

        base.add_path(&other, Some(&Mat::translate(10.0, 5.0, 0.0)));
        assert_eq!(base.segs[0], Seg::Move(0.0, 0.0), "the existing subpath must not move");
        assert_eq!(base.segs[1], Seg::Move(11.0, 5.0));
        assert_eq!(base.segs[2], Seg::Line(12.0, 5.0));
    }

    #[test]
    fn add_path_copies_rather_than_aliases() {
        let mut base = Path::new();
        let mut other = Path::new();
        other.move_to(1.0, 1.0);
        base.add_path(&other, None);
        other.line_to(9.0, 9.0);
        assert_eq!(base.segs.len(), 1, "later edits to the source leaked in");
    }

    #[test]
    fn round_rect_radii_are_scaled_down_to_fit() {
        // Spec: one common factor scales every radius, so a 100-radius corner on
        // a 10-wide box does not wrap around and invert the shape.
        let mut p = Path::new();
        p.round_rect(0.0, 0.0, 10.0, 10.0, [(100.0, 100.0); 4]).unwrap();
        for seg in &p.segs {
            let pt = match seg {
                Seg::Move(x, y) | Seg::Line(x, y) => (*x, *y),
                Seg::Cubic(c) => (c[4], c[5]),
                Seg::Close => continue,
            };
            assert!(
                (-0.001..=10.001).contains(&pt.0) && (-0.001..=10.001).contains(&pt.1),
                "point escaped the box: {pt:?}"
            );
        }
    }

    #[test]
    fn round_rect_rejects_negative_radii() {
        let mut p = Path::new();
        assert!(p.round_rect(0.0, 0.0, 10.0, 10.0, [(-1.0, 1.0); 4]).is_err());
    }

    #[test]
    fn arc_to_with_collinear_points_is_a_line() {
        let mut p = Path::new();
        p.move_to(0.0, 0.0);
        p.arc_to(10.0, 0.0, 20.0, 0.0, 5.0).unwrap();
        assert_eq!(p.segs, vec![Seg::Move(0., 0.), Seg::Line(10., 0.)]);
    }

    #[test]
    fn arc_to_on_an_empty_path_just_moves() {
        let mut p = Path::new();
        p.arc_to(3.0, 4.0, 20.0, 0.0, 5.0).unwrap();
        assert_eq!(p.segs, vec![Seg::Move(3.0, 4.0)]);
    }

    #[test]
    fn the_segment_read_out_round_trips_every_primitive() {
        let mut p = Path::new();
        p.move_to(1.0, 2.0);
        p.line_to(3.0, 4.0);
        p.cubic_to(5.0, 6.0, 7.0, 8.0, 9.0, 10.0);
        p.close();
        assert_eq!(p.to_segments(), vec![0., 1., 2., 1., 3., 4., 2., 5., 6., 7., 8., 9., 10., 3.]);
    }
}

#[cfg(test)]
mod jsc_tests {
    use crate::webapi::testing::*;

    #[test]
    fn a_path_is_constructible_and_branded() {
        let c = ctx();
        ok(&c, "new Path2D() instanceof Path2D");
        let msg = throws(&c, "Path2D.prototype.moveTo.call({}, 1, 2)");
        assert!(msg.contains("non-Path2D"), "{msg}");
    }

    #[test]
    fn the_svg_constructor_and_the_method_calls_agree() {
        let c = ctx();
        ok(
            &c,
            "(() => { const a = new Path2D('M10 10 L20 20'); const b = new Path2D(); \
              b.moveTo(10,10); b.lineTo(20,20); \
              return String(a.toSegments()) === String(b.toSegments()); })()",
        );
    }

    #[test]
    fn a_copy_constructor_snapshots_the_source() {
        let c = ctx();
        ok(
            &c,
            "(() => { const a = new Path2D(); a.moveTo(1,1); const b = new Path2D(a); \
              a.lineTo(5,5); return b.toSegments().length === 3; })()",
        );
    }

    #[test]
    fn add_path_applies_a_dommatrix() {
        // The two halves of this module pair meeting: a DOMMatrix from the
        // geometry module driving a path transform.
        let c = ctx();
        ok(
            &c,
            "(() => { const src = new Path2D(); src.moveTo(1,0); \
              const dst = new Path2D(); dst.addPath(src, new DOMMatrix().translateSelf(10,5)); \
              const s = dst.toSegments(); return s[1] === 11 && s[2] === 5; })()",
        );
    }

    #[test]
    fn a_negative_radius_throws_with_the_spec_error_name() {
        let c = ctx();
        ok(
            &c,
            "(() => { try { new Path2D().arc(0,0,-1,0,1); return false; } \
              catch (e) { return e instanceof DOMException && e.name === 'IndexSizeError' && e.code === 1; } })()",
        );
        ok(
            &c,
            "(() => { try { new Path2D().roundRect(0,0,1,1,[1,2,3,4,5]); return false; } \
              catch (e) { return e instanceof RangeError; } })()",
        );
    }
}
