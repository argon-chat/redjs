//! `RedPhysics3D` — the native Rapier3D world exposed to JS as a host class.
//! One JS instance = one physics world. The TS adapter (`@engine/physics3d-native`)
//! implements `IPhysicsEngine3D` over this surface; semantics deliberately mirror
//! the WASM adapter (`packages/physics3d-rapier3d`) so web and native behave the
//! same, exactly as the 2D pair does.
//!
//! Architecture is a 1:1 port of [`super::p2d`] minus the T1 scope cuts (no joints,
//! no gravity/buoyancy/wind zones, no surface effectors, no character controller,
//! no one-way platforms) plus the quaternion pose lanes 3D needs:
//! - descriptors come in through the shared f64 `scratch` buffer (layouts below);
//!   variable-size data (hull points, trimesh vertices/indices) as typed-array args;
//! - `step(dt)` = ONE rapier step (the fixed-timestep accumulator lives in the
//!   TS adapter) and returns the event-record count;
//! - poses/events/queries are Rust-owned buffers read zero-copy from JS; any
//!   regrow bumps `generation()` and the adapter refetches the views.

use core::ffi::c_void;
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

use rapier3d::prelude::*;

use super::{ExposedF32, ExposedF64, Slab};
use crate::capi;
use crate::webapi::util::{
    cf_arg, cf_new_target, cf_this, dec_f64, define_class, enc_bool, enc_f64, get, host_create,
    host_data, is_cell, make_host_class, throw_type_error, ClassSpec, FALSE, TRUE, UNDEF,
};

// ---- wire constants (mirrored in `@engine/physics3d-native/protocol`) -------

/// poses buffer: slot-indexed, stride 8 = [x, y, z, qx, qy, qz, qw, reserved].
const POSE_STRIDE: usize = 8;
/// events buffer: stride 9 = [kind, bodyA, bodyB, cx, cy, cz, nx, ny, nz].
const EV_STRIDE: usize = 9;
/// event kinds.
const EV_COLL_ENTER: f32 = 0.0;
const EV_COLL_STAY: f32 = 1.0;
const EV_COLL_EXIT: f32 = 2.0;
const EV_TRIG_ENTER: f32 = 3.0;
const EV_TRIG_STAY: f32 = 4.0;
const EV_TRIG_EXIT: f32 = 5.0;
/// raycastAll query records: stride 8 = [bodySlot, px, py, pz, nx, ny, nz, dist].
const RAY_STRIDE: usize = 8;
/// debugRender vertex records (queries buffer): stride 7 = [x, y, z, r, g, b, a],
/// two vertices per line segment; colors already converted HSLA→RGBA.
const DEBUG_STRIDE: usize = 7;

const SCRATCH_LEN: usize = 512;
const INITIAL_POSES: usize = 1024 * POSE_STRIDE;
const INITIAL_EVENTS: usize = 512 * EV_STRIDE;
const INITIAL_QUERIES: usize = 4096;

/// Collider shape discriminants (scratch[0] of `addCollider`).
const SHAPE_BOX: u32 = 0;
const SHAPE_SPHERE: u32 = 1;
const SHAPE_CAPSULE: u32 = 2;
const SHAPE_CONVEX_HULL: u32 = 3;
const SHAPE_TRIMESH: u32 = 4;

/// engine PhysicsBodyType3D: 0 Dynamic / 1 Kinematic / 2 Static.
const BODY_STATIC: u8 = 2;

// Scratch layouts (f64 indices), kept in sync with the TS adapter:
// createBody:   [0]=bodyType(0 dyn/1 kin/2 static) [1..3]=x,y,z [4..7]=qx,qy,qz,qw
//               [8]=mass(0=auto; >0 = authored TOTAL, re-targeted after colliders)
//               [9]=linearDamping [10]=angularDamping [11]=gravityScale
//               [12]=constraints(bit0..2 freeze pos X/Y/Z, bit3..5 freeze rot X/Y/Z)
//               [13]=ccd [14]=dominance [15..16]=sleep thresholds (native-only) [17]=allowSleep
// addCollider:  [0]=shape (0 box/1 sphere/2 capsule/3 convexHull/4 trimesh)
//               [1..3]=dims — box: halfExtents x,y,z | sphere: radius in [1]
//                             capsule: halfHeight in [1], radius in [2] (local Y axis)
//                             convexHull/trimesh: unused (geometry rides the typed-array args)
//               [4..6]=offX,offY,offZ [7..10]=qx,qy,qz,qw (shape-local orientation)
//               [11]=isTrigger [12]=friction [13]=restitution [14]=density
//               [15]=frictionCombine [16]=restitutionCombine (ENGINE CombineMode3D values —
//                     remapped 2↔3 to rapier's CoefficientCombineRule, never a raw cast)
//               [17]=layerMask [18]=collisionMask
// raycast out:  [0..2]=px,py,pz [3..5]=nx,ny,nz [6]=dist [7]=bodySlot
// getLinearVelocity / getAngularVelocity out: [0..2]=x,y,z
// diagnostics out: [0]=bodyCount [1]=colliderCount [2]=activeBodies [3]=sleepingBodies

const CREATE_BODY_FIELDS: usize = 18;
const ADD_COLLIDER_FIELDS: usize = 19;

// ---- world state ------------------------------------------------------------

struct BodyRec {
    handle: RigidBodyHandle,
    /// engine PhysicsBodyType3D: 0 Dynamic / 1 Kinematic / 2 Static.
    body_type: u8,
    /// Last additional mass we set (rapier REPLACES it on set — needed for drift-free setMass).
    additional_mass: f32,
    /// Authored total mass (>0 = Mass field set; re-targeted after colliders attach).
    explicit_mass: f32,
}

struct ColRec {
    handle: ColliderHandle,
    body_id: u32,
}

/// Raw event captured during the step; body slots + contact data resolved at
/// event time (the collider may be gone by the time we post-process REMOVED
/// stops — the active-pair maps hold the slot fallback).
struct RawEv {
    started: bool,
    sensor: bool,
    h1: ColliderHandle,
    h2: ColliderHandle,
    a: u32,
    b: u32,
    /// [px, py, pz, nx, ny, nz], world space.
    contact: [f32; 6],
}

#[derive(Default)]
struct Collector {
    evs: Mutex<Vec<RawEv>>,
}

impl EventHandler for Collector {
    fn handle_collision_event(
        &self,
        bodies: &RigidBodySet,
        colliders: &ColliderSet,
        event: CollisionEvent,
        contact_pair: Option<&ContactPair>,
    ) {
        let (h1, h2, started, flags) = match event {
            CollisionEvent::Started(a, b, f) => (a, b, true, f),
            CollisionEvent::Stopped(a, b, f) => (a, b, false, f),
        };
        let slot_of = |h: ColliderHandle| -> u32 {
            colliders
                .get(h)
                .and_then(|c| c.parent())
                .and_then(|bh| bodies.get(bh))
                .map(|b| b.user_data as u32)
                .unwrap_or(u32::MAX)
        };
        let contact = contact_pair
            .map(|p| pair_contact(colliders, p))
            .unwrap_or([0.0; 6]);
        self.evs.lock().unwrap().push(RawEv {
            started,
            sensor: flags.contains(CollisionEventFlags::SENSOR),
            h1,
            h2,
            a: slot_of(h1),
            b: slot_of(h2),
            contact,
        });
    }

    fn handle_contact_force_event(
        &self,
        _dt: Real,
        _bodies: &RigidBodySet,
        _colliders: &ColliderSet,
        _contact_pair: &ContactPair,
        _total_force_magnitude: Real,
    ) {
    }
}

/// Deepest contact point (world) + world normal of a contact pair.
fn pair_contact(colliders: &ColliderSet, pair: &ContactPair) -> [f32; 6] {
    if let Some((manifold, contact)) = pair.find_deepest_contact() {
        let n = manifold.data.normal;
        if let Some(co1) = colliders.get(pair.collider1) {
            let p = co1.position().transform_point(contact.local_p1);
            return [p.x, p.y, p.z, n.x, n.y, n.z];
        }
    }
    [0.0; 6]
}

type PairKey = u64;

fn pair_key(h1: ColliderHandle, h2: ColliderHandle) -> PairKey {
    let (i1, _) = h1.into_raw_parts();
    let (i2, _) = h2.into_raw_parts();
    let (lo, hi) = if i1 <= i2 { (i1, i2) } else { (i2, i1) };
    ((hi as u64) << 32) | lo as u64
}

struct ActivePair {
    a: u32,
    b: u32,
    h1: ColliderHandle,
    h2: ColliderHandle,
}

fn ev_record(kind: f32, a: u32, b: u32, c: [f32; 6]) -> [f32; EV_STRIDE] {
    [kind, a as f32, b as f32, c[0], c[1], c[2], c[3], c[4], c[5]]
}

/// Contact/trigger pair bookkeeping — split out of [`WorldData`] so the enter/exit/stay
/// fold is a plain function over plain data (unit-tested without a JS context).
#[derive(Default)]
struct EventState {
    active_contacts: HashMap<PairKey, ActivePair>,
    active_triggers: HashMap<PairKey, ActivePair>,
    /// Exit records synthesized by removeBody/removeCollider mid-frame (slots
    /// still valid at synthesis time); prepended to the next step's events.
    /// Rapier's own Stopped(REMOVED) for these pairs arrives a step later, when
    /// the slot may already be REUSED by a new body — dispatching then would
    /// name the wrong entity.
    pending_exits: Vec<[f32; EV_STRIDE]>,
}

impl EventState {
    fn clear(&mut self) {
        self.active_contacts.clear();
        self.active_triggers.clear();
        self.pending_exits.clear();
    }

    /// Removes active contact/trigger pairs matching `involves`, staging exit
    /// records with the still-valid slot ids. Called at removal time — waiting for
    /// rapier's Stopped(REMOVED) a step later risks the slot being reused by a new
    /// body and the exit naming the wrong entity.
    fn purge_pairs_involving(&mut self, involves: impl Fn(&ActivePair) -> bool) {
        let keys: Vec<PairKey> = self
            .active_contacts
            .iter()
            .filter(|(_, p)| involves(p))
            .map(|(k, _)| *k)
            .collect();
        for k in keys {
            if let Some(p) = self.active_contacts.remove(&k) {
                self.pending_exits
                    .push(ev_record(EV_COLL_EXIT, p.a, p.b, [0.0; 6]));
            }
        }
        let keys: Vec<PairKey> = self
            .active_triggers
            .iter()
            .filter(|(_, p)| involves(p))
            .map(|(k, _)| *k)
            .collect();
        for k in keys {
            if let Some(p) = self.active_triggers.remove(&k) {
                self.pending_exits
                    .push(ev_record(EV_TRIG_EXIT, p.a, p.b, [0.0; 6]));
            }
        }
    }

    /// Folds one step's raw events into `out`: pending exits first, then
    /// enter/exit against the active maps, then a stay for every still-active
    /// pair that did NOT enter this step (WASM-adapter semantics). `probe`
    /// re-reads the deepest contact of a live collision pair.
    fn fold(
        &mut self,
        drained: Vec<RawEv>,
        out: &mut Vec<[f32; EV_STRIDE]>,
        probe: &dyn Fn(ColliderHandle, ColliderHandle) -> [f32; 6],
    ) {
        out.clear();
        // Exits staged by removeBody/removeCollider since the last step go first
        // (their slots were resolved at removal time, before any reuse).
        out.append(&mut self.pending_exits);
        // Pairs that ENTERED this step — their stay starts next step (WASM parity).
        let mut entered: HashSet<PairKey> = HashSet::new();
        for ev in drained {
            let key = pair_key(ev.h1, ev.h2);
            if ev.started {
                if ev.a == u32::MAX || ev.b == u32::MAX {
                    continue;
                }
                let pair = ActivePair { a: ev.a, b: ev.b, h1: ev.h1, h2: ev.h2 };
                let kind = if ev.sensor { EV_TRIG_ENTER } else { EV_COLL_ENTER };
                out.push(ev_record(kind, ev.a, ev.b, ev.contact));
                entered.insert(key);
                if ev.sensor {
                    self.active_triggers.insert(key, pair);
                } else {
                    self.active_contacts.insert(key, pair);
                }
            } else {
                let map = if ev.sensor {
                    &mut self.active_triggers
                } else {
                    &mut self.active_contacts
                };
                if let Some(p) = map.remove(&key) {
                    let kind = if ev.sensor { EV_TRIG_EXIT } else { EV_COLL_EXIT };
                    out.push(ev_record(kind, p.a, p.b, ev.contact));
                }
            }
        }
        for (key, p) in self.active_contacts.iter() {
            if entered.contains(key) {
                continue;
            }
            out.push(ev_record(EV_COLL_STAY, p.a, p.b, probe(p.h1, p.h2)));
        }
        for (key, p) in self.active_triggers.iter() {
            if entered.contains(key) {
                continue;
            }
            out.push(ev_record(EV_TRIG_STAY, p.a, p.b, [0.0; 6]));
        }
    }
}

struct WorldData {
    dead: bool,
    world: PhysicsWorld,
    collector: Collector,
    bodies: Slab<BodyRec>,
    colliders: Slab<ColRec>,
    events_enabled: HashSet<u32>,
    ev_state: EventState,
    /// Bodies given applyForce/applyTorque since the last step — reset after it
    /// (one-shot semantics; rapier user forces otherwise persist forever).
    forced_bodies: HashSet<RigidBodyHandle>,
    poses: ExposedF32,
    /// Previous fixed-step poses (same slot layout) — render-rate interpolation
    /// blends prev→curr TS-side using the accumulator remainder.
    prev_poses: ExposedF32,
    events: ExposedF32,
    queries: ExposedF32,
    scratch: ExposedF64,
    generation: u32,
    ev_staging: Vec<[f32; EV_STRIDE]>,
    /// Reused id scratch — two-pass writes that would otherwise alias `world`.
    id_scratch: Vec<u32>,
}

impl WorldData {
    unsafe fn new(g: *mut c_void) -> Self {
        WorldData {
            dead: false,
            world: PhysicsWorld::new(),
            collector: Collector::default(),
            bodies: Slab::new(),
            colliders: Slab::new(),
            events_enabled: Default::default(),
            ev_state: EventState::default(),
            forced_bodies: Default::default(),
            poses: ExposedF32::new(g, INITIAL_POSES),
            prev_poses: ExposedF32::new(g, INITIAL_POSES),
            events: ExposedF32::new(g, INITIAL_EVENTS),
            queries: ExposedF32::new(g, INITIAL_QUERIES),
            scratch: ExposedF64::new(g, SCRATCH_LEN),
            generation: 0,
            ev_staging: Vec::new(),
            id_scratch: Vec::new(),
        }
    }

    fn body(&self, id: u32) -> Option<RigidBodyHandle> {
        self.bodies.get(id).map(|r| r.handle)
    }
}

// ---- per-context class state ------------------------------------------------

struct ClassState {
    cls: capi::JSClassRef,
    proto: u64,
}

struct SendState(ClassState);
unsafe impl Send for SendState {}

fn states() -> &'static Mutex<HashMap<usize, SendState>> {
    static S: OnceLock<Mutex<HashMap<usize, SendState>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

fn class_of(g: *mut c_void) -> Option<(capi::JSClassRef, u64)> {
    states().lock().unwrap().get(&(g as usize)).map(|s| (s.0.cls, s.0.proto))
}

macro_rules! world {
    ($g:expr, $cf:expr) => {{
        let Some((cls, _)) = class_of($g) else {
            return throw_type_error($g, "RedPhysics3D not installed");
        };
        match host_data::<WorldData>($g, cls, cf_this($cf)) {
            Some(w) if !w.dead => w,
            Some(_) => return UNDEF,
            None => return throw_type_error($g, "RedPhysics3D method called on wrong this"),
        }
    }};
}

// ---- arg helpers ------------------------------------------------------------

unsafe fn arg_f(cf: *mut c_void, i: u32) -> f64 {
    dec_f64(cf_arg(cf, i)).unwrap_or(0.0)
}

unsafe fn arg_u(cf: *mut c_void, i: u32) -> u32 {
    arg_f(cf, i) as u32
}

/// Reads a Float32Array argument as xyz triples (copied — safe across JS calls).
unsafe fn arg_verts(g: *mut c_void, cf: *mut c_void, i: u32) -> Vec<Vector> {
    let v = cf_arg(cf, i);
    if !is_cell(v) {
        return Vec::new();
    }
    let mut ta = crate::ffi::RjsTaInfo::default();
    // kind 8 = Float32Array.
    if crate::glue::rjs_typedarray_info(g, v, &mut ta) != 0 || ta.kind != 8 || ta.data.is_null() {
        return Vec::new();
    }
    let floats = core::slice::from_raw_parts(ta.data as *const f32, ta.length as usize);
    floats
        .chunks_exact(3)
        .map(|c| Vector::new(c[0], c[1], c[2]))
        .collect()
}

/// Reads a Uint32Array argument as triangle index triples.
unsafe fn arg_indices(g: *mut c_void, cf: *mut c_void, i: u32) -> Vec<[u32; 3]> {
    let v = cf_arg(cf, i);
    if !is_cell(v) {
        return Vec::new();
    }
    let mut ta = crate::ffi::RjsTaInfo::default();
    // kind 7 = Uint32Array.
    if crate::glue::rjs_typedarray_info(g, v, &mut ta) != 0 || ta.kind != 7 || ta.data.is_null() {
        return Vec::new();
    }
    let idx = core::slice::from_raw_parts(ta.data as *const u32, ta.length as usize);
    idx.chunks_exact(3).map(|c| [c[0], c[1], c[2]]).collect()
}

/// ⚠ engine CombineMode3D orders Average=0 Min=1 **Max=2 Multiply=3**; rapier's
/// CoefficientCombineRule orders Average=0 Min=1 **Multiply=2 Max=3**. A raw cast
/// silently swaps Max and Multiply — this remap is the whole point of the function.
fn combine_rule(v: u32) -> CoefficientCombineRule {
    match v {
        1 => CoefficientCombineRule::Min,
        2 => CoefficientCombineRule::Max,
        3 => CoefficientCombineRule::Multiply,
        _ => CoefficientCombineRule::Average,
    }
}

fn groups(layer_mask: u32, collision_mask: u32) -> InteractionGroups {
    InteractionGroups::all()
        .with_memberships(Group::from_bits_truncate(layer_mask))
        .with_filter(Group::from_bits_truncate(collision_mask))
}

fn query_filter<'a>(mask: u32) -> QueryFilter<'a> {
    let mut f = QueryFilter::default();
    if mask != u32::MAX {
        f.groups = Some(InteractionGroups::all().with_filter(Group::from_bits_truncate(mask)));
    }
    f
}

/// Constraints bitfield → rapier locked axes: bit0..2 freeze position X/Y/Z,
/// bit3..5 freeze rotation X/Y/Z (the same bit order rapier itself uses).
fn locked_axes_from_bits(bits: u32) -> LockedAxes {
    let mut locked = LockedAxes::empty();
    if bits & (1 << 0) != 0 {
        locked |= LockedAxes::TRANSLATION_LOCKED_X;
    }
    if bits & (1 << 1) != 0 {
        locked |= LockedAxes::TRANSLATION_LOCKED_Y;
    }
    if bits & (1 << 2) != 0 {
        locked |= LockedAxes::TRANSLATION_LOCKED_Z;
    }
    if bits & (1 << 3) != 0 {
        locked |= LockedAxes::ROTATION_LOCKED_X;
    }
    if bits & (1 << 4) != 0 {
        locked |= LockedAxes::ROTATION_LOCKED_Y;
    }
    if bits & (1 << 5) != 0 {
        locked |= LockedAxes::ROTATION_LOCKED_Z;
    }
    locked
}

/// Unit quaternion from wire lanes. A zero/NaN quat (unset authoring data) would
/// poison every pose derived from it, so it degrades to identity instead.
fn quat_from(x: f64, y: f64, z: f64, w: f64) -> Rotation {
    let q = Rotation::from_xyzw(x as f32, y as f32, z as f32, w as f32);
    let len_sq = q.length_squared();
    if len_sq.is_finite() && len_sq > 1e-12 {
        q.normalize()
    } else {
        Rotation::IDENTITY
    }
}

/// Writes one pose row: [x, y, z, qx, qy, qz, qw, reserved].
fn write_pose(buf: &mut [f32], slot: u32, t: Vector, r: Rotation) {
    let base = slot as usize * POSE_STRIDE;
    if base + POSE_STRIDE > buf.len() {
        return;
    }
    buf[base] = t.x;
    buf[base + 1] = t.y;
    buf[base + 2] = t.z;
    buf[base + 3] = r.x;
    buf[base + 4] = r.y;
    buf[base + 5] = r.z;
    buf[base + 6] = r.w;
    buf[base + 7] = 0.0;
}

/// Decodes the `createBody` scratch layout. Returns the builder plus the fields
/// the slot record keeps (body type, authored total mass).
fn body_builder_from_scratch(s: &[f64]) -> (RigidBodyBuilder, u8, f32) {
    let body_type = s[0] as u8;
    let builder = match body_type {
        1 => RigidBodyBuilder::kinematic_position_based(),
        2 => RigidBodyBuilder::fixed(),
        _ => RigidBodyBuilder::dynamic(),
    };
    let mut builder = builder
        .pose(Pose::from_parts(
            Vector::new(s[1] as f32, s[2] as f32, s[3] as f32),
            quat_from(s[4], s[5], s[6], s[7]),
        ))
        .linear_damping(s[9] as f32)
        .angular_damping(s[10] as f32)
        .gravity_scale(s[11] as f32)
        .ccd_enabled(s[13] != 0.0)
        .dominance_group((s[14] as i32).clamp(-127, 127) as i8)
        .can_sleep(s[17] != 0.0);
    let explicit_mass = if s[8] > 0.0 { s[8] as f32 } else { 0.0 };
    if explicit_mass > 0.0 {
        builder = builder.additional_mass(explicit_mass);
    }
    let locked = locked_axes_from_bits(s[12] as u32);
    if !locked.is_empty() {
        builder = builder.locked_axes(locked);
    }
    (builder, body_type, explicit_mass)
}

/// Why a collider descriptor could not become a rapier shape. Every variant is a
/// loud-and-skip: the level keeps populating, but nothing ships a wrong shape.
#[derive(Debug, PartialEq, Eq)]
enum ShapeError {
    /// Fewer than 4 points, or all of them coplanar — no hull exists.
    DegenerateHull,
    /// Concave triangle soup on a non-fixed body: rapier has no inertia tensor for
    /// it, so the body would behave as if it had no collider at all.
    TrimeshOnNonFixedBody,
    /// Empty triangle list or an index past the end of the vertex array.
    BadTrimesh,
    /// Shape discriminant this host does not know — a TS/Rust version skew. Refusing
    /// beats the 2D-style box fallback, which would silently ship wrong geometry.
    UnknownShape,
}

impl ShapeError {
    fn as_str(&self) -> &'static str {
        match self {
            ShapeError::DegenerateHull => "degenerate convex hull (need ≥4 non-coplanar points)",
            ShapeError::TrimeshOnNonFixedBody => {
                "trimesh collider on a non-static body — use convexHull for dynamic geometry"
            }
            ShapeError::BadTrimesh => "malformed trimesh (empty or out-of-range indices)",
            ShapeError::UnknownShape => "unknown collider shape discriminant",
        }
    }
}

/// Decodes the `addCollider` scratch layout into a builder. `verts`/`indices` carry
/// convexHull/trimesh geometry; `body_type` gates the trimesh restriction.
fn collider_builder_from_scratch(
    s: &[f64],
    verts: Vec<Vector>,
    indices: Vec<[u32; 3]>,
    body_type: u8,
) -> Result<ColliderBuilder, ShapeError> {
    let shape = s[0] as u32;
    let builder = match shape {
        SHAPE_SPHERE => ColliderBuilder::ball((s[1] as f32).max(1e-4)),
        SHAPE_CAPSULE => {
            ColliderBuilder::capsule_y((s[1] as f32).max(0.0), (s[2] as f32).max(1e-4))
        }
        SHAPE_CONVEX_HULL => {
            ColliderBuilder::convex_hull(&verts).ok_or(ShapeError::DegenerateHull)?
        }
        SHAPE_TRIMESH => {
            if body_type != BODY_STATIC {
                return Err(ShapeError::TrimeshOnNonFixedBody);
            }
            let n = verts.len() as u32;
            if indices.is_empty() || indices.iter().any(|t| t.iter().any(|i| *i >= n)) {
                return Err(ShapeError::BadTrimesh);
            }
            ColliderBuilder::trimesh(verts, indices).map_err(|_| ShapeError::BadTrimesh)?
        }
        SHAPE_BOX => ColliderBuilder::cuboid(
            (s[1] as f32).max(1e-4),
            (s[2] as f32).max(1e-4),
            (s[3] as f32).max(1e-4),
        ),
        _ => return Err(ShapeError::UnknownShape),
    };
    let is_trigger = s[11] != 0.0;
    let mut builder = builder
        .position(Pose::from_parts(
            Vector::new(s[4] as f32, s[5] as f32, s[6] as f32),
            quat_from(s[7], s[8], s[9], s[10]),
        ))
        .sensor(is_trigger)
        .friction(s[12] as f32)
        .restitution(s[13] as f32)
        .density((s[14] as f32).max(1e-4))
        .friction_combine_rule(combine_rule(s[15] as u32))
        .restitution_combine_rule(combine_rule(s[16] as u32))
        .collision_groups(groups(s[17] as u32, s[18] as u32));
    if is_trigger {
        builder = builder.active_events(ActiveEvents::COLLISION_EVENTS);
    }
    Ok(builder)
}

// ---- lifecycle --------------------------------------------------------------

unsafe extern "sysv64" fn ctor(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some((cls, default_proto)) = class_of(g) else {
        return throw_type_error(g, "RedPhysics3D not installed");
    };
    let mut proto = get(g, cf_new_target(cf), "prototype");
    if !is_cell(proto) {
        proto = default_proto;
    }
    let wd = WorldData::new(g);
    host_create::<WorldData>(g, cls, proto, wd)
}

unsafe extern "sysv64" fn free(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some((cls, _)) = class_of(g) else { return UNDEF };
    let Some(wd) = host_data::<WorldData>(g, cls, cf_this(cf)) else {
        return throw_type_error(g, "RedPhysics3D method called on wrong this");
    };
    if wd.dead {
        return UNDEF;
    }
    wd.dead = true;
    wd.poses.release(g);
    wd.prev_poses.release(g);
    wd.events.release(g);
    wd.queries.release(g);
    wd.scratch.release(g);
    wd.world = PhysicsWorld::new();
    wd.bodies.clear();
    wd.colliders.clear();
    wd.events_enabled.clear();
    wd.ev_state.clear();
    wd.forced_bodies.clear();
    UNDEF
}

// ---- views / config ---------------------------------------------------------

unsafe extern "sysv64" fn generation(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    enc_f64(wd.generation as f64)
}

unsafe extern "sysv64" fn get_poses(g: *mut c_void, cf: *mut c_void) -> u64 {
    world!(g, cf).poses.view()
}

unsafe extern "sysv64" fn get_poses_prev(g: *mut c_void, cf: *mut c_void) -> u64 {
    world!(g, cf).prev_poses.view()
}

unsafe extern "sysv64" fn get_events(g: *mut c_void, cf: *mut c_void) -> u64 {
    world!(g, cf).events.view()
}

unsafe extern "sysv64" fn get_queries(g: *mut c_void, cf: *mut c_void) -> u64 {
    world!(g, cf).queries.view()
}

unsafe extern "sysv64" fn get_scratch(g: *mut c_void, cf: *mut c_void) -> u64 {
    world!(g, cf).scratch.view()
}

unsafe extern "sysv64" fn set_gravity(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    wd.world.gravity = Vector::new(arg_f(cf, 0) as f32, arg_f(cf, 1) as f32, arg_f(cf, 2) as f32);
    UNDEF
}

unsafe extern "sysv64" fn set_timestep(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let dt = arg_f(cf, 0) as f32;
    if dt > 0.0 {
        wd.world.integration_parameters.dt = dt;
    }
    UNDEF
}

unsafe extern "sysv64" fn set_solver_iterations(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let n = arg_u(cf, 0) as usize;
    if n > 0 {
        wd.world.integration_parameters.num_solver_iterations = n;
    }
    UNDEF
}

// ---- bodies -----------------------------------------------------------------

/// Rewrites every live body's pose row. Regrown buffers start zeroed — without
/// a full reseed, sleeping bodies would read the origin with a zero quat until
/// they wake (the active-bodies sync in step() never rewrites them).
fn reseed_all_poses(wd: &mut WorldData) {
    wd.id_scratch.clear();
    for (id, _) in wd.bodies.iter() {
        wd.id_scratch.push(id);
    }
    for i in 0..wd.id_scratch.len() {
        let id = wd.id_scratch[i];
        let Some(handle) = wd.body(id) else { continue };
        let Some(rb) = wd.world.bodies.get(handle) else { continue };
        let t = rb.translation();
        let r = *rb.rotation();
        write_pose(wd.poses.data(), id, t, r);
        write_pose(wd.prev_poses.data(), id, t, r);
    }
}

unsafe extern "sysv64" fn create_body(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let s: Vec<f64> = wd.scratch.data()[..CREATE_BODY_FIELDS].to_vec();
    let (builder, body_type, explicit_mass) = body_builder_from_scratch(&s);
    let id = wd.bodies.insert(BodyRec {
        handle: RigidBodyHandle::invalid(),
        body_type,
        additional_mass: explicit_mass,
        explicit_mass,
    });
    let handle = wd.world.insert_body(builder.user_data(id as u128));
    wd.bodies.get_mut(id).unwrap().handle = handle;
    // Sleep thresholds (native-only knobs — rapier.js does not expose these).
    if s[15] > 0.0 || s[16] > 0.0 {
        if let Some(rb) = wd.world.bodies.get_mut(handle) {
            let act = rb.activation_mut();
            if s[15] > 0.0 {
                act.normalized_linear_threshold = s[15] as f32;
            }
            if s[16] > 0.0 {
                act.angular_threshold = s[16] as f32;
            }
        }
    }
    // Seed the pose buffers so the slot reads correctly before the first step.
    let needed = (id as usize + 1) * POSE_STRIDE;
    let regrew = wd.poses.ensure(g, needed);
    let regrew_prev = wd.prev_poses.ensure(g, needed);
    if regrew || regrew_prev {
        wd.generation += 1;
        // Regrown buffer starts zeroed — reseed EVERY live body, not just the
        // new one (step()'s own regrow branch can't fire after this ensure()
        // already grew the buffer; sleeping bodies would snap to the origin).
        reseed_all_poses(wd);
    }
    let t = Vector::new(s[1] as f32, s[2] as f32, s[3] as f32);
    let r = quat_from(s[4], s[5], s[6], s[7]);
    write_pose(wd.poses.data(), id, t, r);
    write_pose(wd.prev_poses.data(), id, t, r);
    enc_f64(id as f64)
}

unsafe extern "sysv64" fn remove_body(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let id = arg_u(cf, 0);
    let Some(rec) = wd.bodies.remove(id) else { return UNDEF };
    // Remove collider slots owned by this body (rapier removes the actual
    // colliders together with the body).
    let owned: Vec<(u32, ColliderHandle)> = wd
        .colliders
        .iter()
        .filter(|(_, c)| c.body_id == id)
        .map(|(cid, c)| (cid, c.handle))
        .collect();
    // Stage exits for pairs this body is touching BEFORE its slot can be reused.
    wd.ev_state
        .purge_pairs_involving(|p| owned.iter().any(|(_, h)| *h == p.h1 || *h == p.h2));
    for (cid, _) in owned {
        wd.colliders.remove(cid);
    }
    wd.events_enabled.remove(&id);
    wd.forced_bodies.remove(&rec.handle);
    wd.world.remove_body(rec.handle);
    UNDEF
}

// ---- colliders --------------------------------------------------------------

unsafe extern "sysv64" fn add_collider(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let body_id = arg_u(cf, 0);
    let Some((body, body_type)) = wd.bodies.get(body_id).map(|r| (r.handle, r.body_type)) else {
        return enc_f64(-1.0);
    };
    let s: Vec<f64> = wd.scratch.data()[..ADD_COLLIDER_FIELDS].to_vec();
    let shape = s[0] as u32;
    // Geometry only rides the typed-array args for the mesh shapes.
    let (verts, indices) = if shape == SHAPE_CONVEX_HULL || shape == SHAPE_TRIMESH {
        (arg_verts(g, cf, 1), arg_indices(g, cf, 2))
    } else {
        (Vec::new(), Vec::new())
    };
    let builder = match collider_builder_from_scratch(&s, verts, indices, body_type) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[physics3d] addCollider refused: {}", e.as_str());
            return enc_f64(-1.0);
        }
    };
    let id = wd.colliders.insert(ColRec { handle: ColliderHandle::invalid(), body_id });
    let handle = wd
        .world
        .insert_collider(builder.user_data(id as u128), Some(body));
    wd.colliders.get_mut(id).unwrap().handle = handle;
    // Body with events enabled → new collider inherits the flag.
    if wd.events_enabled.contains(&body_id) {
        if let Some(co) = wd.world.colliders.get_mut(handle) {
            co.set_active_events(ActiveEvents::COLLISION_EVENTS);
        }
    }
    // Authored Mass means TOTAL mass — re-target after this collider added its
    // density-derived contribution.
    let (explicit, stored) = wd
        .bodies
        .get(body_id)
        .map(|r| (r.explicit_mass, r.additional_mass))
        .unwrap_or((0.0, 0.0));
    if explicit > 0.0 {
        if let Some(rb) = wd.world.bodies.get_mut(body) {
            rb.recompute_mass_properties_from_colliders(&wd.world.colliders);
            let base = (rb.mass() - stored).max(0.0);
            let add = explicit - base;
            rb.set_additional_mass(add, true);
            if let Some(rec) = wd.bodies.get_mut(body_id) {
                rec.additional_mass = add;
            }
        }
    }
    enc_f64(id as f64)
}

unsafe extern "sysv64" fn remove_collider(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let id = arg_u(cf, 0);
    let Some(rec) = wd.colliders.remove(id) else { return UNDEF };
    let handle = rec.handle;
    wd.ev_state
        .purge_pairs_involving(|p| p.h1 == handle || p.h2 == handle);
    wd.world.remove_collider(handle);
    UNDEF
}

/// setBodyEventsEnabled(bodyId, enabled) — mirrors the WASM adapter's
/// setEventHandler: collision/trigger events only flow for opted-in bodies.
unsafe extern "sysv64" fn set_body_events_enabled(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let body_id = arg_u(cf, 0);
    let enabled = cf_arg(cf, 1) == TRUE || arg_f(cf, 1) != 0.0;
    if enabled {
        wd.events_enabled.insert(body_id);
    } else {
        wd.events_enabled.remove(&body_id);
    }
    let handles: Vec<ColliderHandle> = wd
        .colliders
        .iter()
        .filter(|(_, c)| c.body_id == body_id)
        .map(|(_, c)| c.handle)
        .collect();
    for h in handles {
        if let Some(co) = wd.world.colliders.get_mut(h) {
            let ev = if enabled || co.is_sensor() {
                ActiveEvents::COLLISION_EVENTS
            } else {
                ActiveEvents::empty()
            };
            co.set_active_events(ev);
        }
    }
    UNDEF
}

// ---- step -------------------------------------------------------------------

unsafe extern "sysv64" fn step(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let dt = arg_f(cf, 0) as f32;
    if dt <= 0.0 {
        return enc_f64(0.0);
    }
    wd.world.integration_parameters.dt = dt;

    // No physics hooks in T1 (one-way platforms are the 2D-only feature that
    // needs them) — `()` is rapier's no-op PhysicsHooks.
    wd.world.step_with_events(&(), &wd.collector);

    // One-shot force semantics (Unity parity): rapier's add_force/add_torque
    // persist until reset — clear them after the step they were applied to.
    if !wd.forced_bodies.is_empty() {
        let handles: Vec<RigidBodyHandle> = wd.forced_bodies.drain().collect();
        for h in handles {
            if let Some(rb) = wd.world.bodies.get_mut(h) {
                rb.reset_forces(false);
                rb.reset_torques(false);
            }
        }
    }

    // ── event pipeline: drain → enter/exit vs active maps → synthesize stay ──
    let drained: Vec<RawEv> = {
        let mut lock = wd.collector.evs.lock().unwrap();
        lock.drain(..).collect()
    };
    {
        // Disjoint field borrows: the fold mutates only the pair maps while the
        // stay probe reads the world.
        let w = &mut *wd;
        let world = &w.world;
        let probe = |h1: ColliderHandle, h2: ColliderHandle| -> [f32; 6] {
            world
                .contact_pair(h1, h2)
                .map(|pair| pair_contact(&world.colliders, pair))
                .unwrap_or([0.0; 6])
        };
        w.ev_state.fold(drained, &mut w.ev_staging, &probe);
    }

    // ── write events buffer ──
    let needed = wd.ev_staging.len() * EV_STRIDE;
    if wd.events.ensure(g, needed) {
        wd.generation += 1;
    }
    {
        let buf = wd.events.data();
        for (i, rec) in wd.ev_staging.iter().enumerate() {
            buf[i * EV_STRIDE..(i + 1) * EV_STRIDE].copy_from_slice(rec);
        }
    }

    // ── write poses for awake bodies (sleeping slots keep their last pose) ──
    let needed = wd.bodies.capacity() * POSE_STRIDE;
    let regrew = wd.poses.ensure(g, needed);
    let regrew_prev = wd.prev_poses.ensure(g, needed);
    if regrew || regrew_prev {
        wd.generation += 1;
        reseed_all_poses(wd);
    } else {
        wd.id_scratch.clear();
        for (_, rb) in wd.world.active_bodies() {
            wd.id_scratch.push(rb.user_data as u32);
        }
        // Two passes to appease the borrow checker: collect ids, then write.
        for i in 0..wd.id_scratch.len() {
            let id = wd.id_scratch[i];
            let Some(handle) = wd.body(id) else { continue };
            let Some(rb) = wd.world.bodies.get(handle) else { continue };
            let t = rb.translation();
            let r = *rb.rotation();
            // Shift the outgoing pose into the prev buffer (interpolation pair).
            let base = id as usize * POSE_STRIDE;
            if base + POSE_STRIDE > wd.poses.len() {
                continue;
            }
            let mut outgoing = [0.0f32; POSE_STRIDE];
            outgoing.copy_from_slice(&wd.poses.data()[base..base + POSE_STRIDE]);
            wd.prev_poses.data()[base..base + POSE_STRIDE].copy_from_slice(&outgoing);
            write_pose(wd.poses.data(), id, t, r);
        }
    }

    enc_f64(wd.ev_staging.len() as f64)
}

// ---- forces / velocity / props / sleep -------------------------------------

macro_rules! with_body_mut {
    ($g:expr, $cf:expr, $wd:ident, $rb:ident, $body:block) => {{
        let $wd = world!($g, $cf);
        let id = arg_u($cf, 0);
        let Some(handle) = $wd.body(id) else { return UNDEF };
        let Some($rb) = $wd.world.bodies.get_mut(handle) else { return UNDEF };
        $body
    }};
}

unsafe fn arg_vec3(cf: *mut c_void, i: u32) -> Vector {
    Vector::new(
        arg_f(cf, i) as f32,
        arg_f(cf, i + 1) as f32,
        arg_f(cf, i + 2) as f32,
    )
}

unsafe extern "sysv64" fn apply_force(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let id = arg_u(cf, 0);
    let Some(handle) = wd.body(id) else { return UNDEF };
    let Some(rb) = wd.world.bodies.get_mut(handle) else { return UNDEF };
    rb.add_force(arg_vec3(cf, 1), true);
    wd.forced_bodies.insert(handle); // one-shot: reset after the next step
    UNDEF
}

unsafe extern "sysv64" fn apply_impulse(g: *mut c_void, cf: *mut c_void) -> u64 {
    with_body_mut!(g, cf, wd, rb, {
        rb.apply_impulse(arg_vec3(cf, 1), true);
        UNDEF
    })
}

unsafe extern "sysv64" fn apply_torque(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let id = arg_u(cf, 0);
    let Some(handle) = wd.body(id) else { return UNDEF };
    let Some(rb) = wd.world.bodies.get_mut(handle) else { return UNDEF };
    rb.add_torque(arg_vec3(cf, 1), true);
    wd.forced_bodies.insert(handle); // one-shot: reset after the next step
    UNDEF
}

unsafe extern "sysv64" fn apply_angular_impulse(g: *mut c_void, cf: *mut c_void) -> u64 {
    with_body_mut!(g, cf, wd, rb, {
        rb.apply_torque_impulse(arg_vec3(cf, 1), true);
        UNDEF
    })
}

unsafe extern "sysv64" fn get_linear_velocity(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let id = arg_u(cf, 0);
    let Some(handle) = wd.body(id) else { return FALSE };
    let Some(rb) = wd.world.bodies.get(handle) else { return FALSE };
    let v = rb.linvel();
    let s = wd.scratch.data();
    s[0] = v.x as f64;
    s[1] = v.y as f64;
    s[2] = v.z as f64;
    TRUE
}

unsafe extern "sysv64" fn set_linear_velocity(g: *mut c_void, cf: *mut c_void) -> u64 {
    with_body_mut!(g, cf, wd, rb, {
        rb.set_linvel(arg_vec3(cf, 1), true);
        UNDEF
    })
}

unsafe extern "sysv64" fn get_angular_velocity(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let id = arg_u(cf, 0);
    let Some(handle) = wd.body(id) else { return FALSE };
    let Some(rb) = wd.world.bodies.get(handle) else { return FALSE };
    let v = rb.angvel();
    let s = wd.scratch.data();
    s[0] = v.x as f64;
    s[1] = v.y as f64;
    s[2] = v.z as f64;
    TRUE
}

unsafe extern "sysv64" fn set_angular_velocity(g: *mut c_void, cf: *mut c_void) -> u64 {
    with_body_mut!(g, cf, wd, rb, {
        rb.set_angvel(arg_vec3(cf, 1), true);
        UNDEF
    })
}

unsafe extern "sysv64" fn set_linear_damping(g: *mut c_void, cf: *mut c_void) -> u64 {
    with_body_mut!(g, cf, wd, rb, {
        rb.set_linear_damping(arg_f(cf, 1) as f32);
        UNDEF
    })
}

unsafe extern "sysv64" fn set_angular_damping(g: *mut c_void, cf: *mut c_void) -> u64 {
    with_body_mut!(g, cf, wd, rb, {
        rb.set_angular_damping(arg_f(cf, 1) as f32);
        UNDEF
    })
}

unsafe extern "sysv64" fn set_gravity_scale(g: *mut c_void, cf: *mut c_void) -> u64 {
    with_body_mut!(g, cf, wd, rb, {
        rb.set_gravity_scale(arg_f(cf, 1) as f32, true);
        UNDEF
    })
}

unsafe extern "sysv64" fn get_mass(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let id = arg_u(cf, 0);
    let m = wd
        .body(id)
        .and_then(|h| wd.world.bodies.get(h))
        .map(|rb| rb.mass())
        .unwrap_or(0.0);
    enc_f64(m as f64)
}

unsafe extern "sysv64" fn set_mass(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let id = arg_u(cf, 0);
    let target = arg_f(cf, 1) as f32;
    let Some(rec) = wd.bodies.get(id) else { return UNDEF };
    let handle = rec.handle;
    let stored = rec.additional_mass;
    let Some(rb) = wd.world.bodies.get_mut(handle) else { return UNDEF };
    // set_additional_mass REPLACES the stored extra — compute against the
    // collider-derived base, not the current total (repeated calls drift).
    rb.recompute_mass_properties_from_colliders(&wd.world.colliders);
    let base = (rb.mass() - stored).max(0.0);
    let add = target - base;
    rb.set_additional_mass(add, true);
    if let Some(rec) = wd.bodies.get_mut(id) {
        rec.additional_mass = add;
    }
    UNDEF
}

unsafe extern "sysv64" fn sleep_body(g: *mut c_void, cf: *mut c_void) -> u64 {
    with_body_mut!(g, cf, wd, rb, {
        rb.sleep();
        UNDEF
    })
}

unsafe extern "sysv64" fn wake_body(g: *mut c_void, cf: *mut c_void) -> u64 {
    with_body_mut!(g, cf, wd, rb, {
        rb.wake_up(true);
        UNDEF
    })
}

unsafe extern "sysv64" fn is_sleeping(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let id = arg_u(cf, 0);
    let v = wd
        .body(id)
        .and_then(|h| wd.world.bodies.get(h))
        .map(|rb| rb.is_sleeping())
        .unwrap_or(false);
    enc_bool(v)
}

/// Kinematic drive: the subsystem pushes authored transforms into
/// kinematic-position-based bodies (next-position target).
unsafe extern "sysv64" fn set_kinematic_target(g: *mut c_void, cf: *mut c_void) -> u64 {
    with_body_mut!(g, cf, wd, rb, {
        rb.set_next_kinematic_position(Pose::from_parts(
            arg_vec3(cf, 1),
            quat_from(arg_f(cf, 4), arg_f(cf, 5), arg_f(cf, 6), arg_f(cf, 7)),
        ));
        UNDEF
    })
}

/// Teleport (editor/scripts moving bodies directly).
unsafe extern "sysv64" fn set_body_position(g: *mut c_void, cf: *mut c_void) -> u64 {
    with_body_mut!(g, cf, wd, rb, {
        rb.set_position(
            Pose::from_parts(
                arg_vec3(cf, 1),
                quat_from(arg_f(cf, 4), arg_f(cf, 5), arg_f(cf, 6), arg_f(cf, 7)),
            ),
            true,
        );
        UNDEF
    })
}

// ---- queries ----------------------------------------------------------------

fn body_slot_of_collider(wd: &WorldData, ch: ColliderHandle) -> f32 {
    wd.world
        .colliders
        .get(ch)
        .and_then(|c| c.parent())
        .and_then(|bh| wd.world.bodies.get(bh))
        .map(|b| b.user_data as u32 as f32)
        .unwrap_or(-1.0)
}

unsafe extern "sysv64" fn raycast(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let origin = arg_vec3(cf, 0);
    let dir = arg_vec3(cf, 3);
    let max_dist = arg_f(cf, 6) as f32;
    let mask = if cf_arg(cf, 7) == UNDEF { u32::MAX } else { arg_u(cf, 7) };
    let len = dir.length();
    if len < 1e-6 || max_dist <= 0.0 {
        return enc_f64(0.0);
    }
    let ray = Ray::new(origin, dir / len);
    let hit = wd
        .world
        .cast_ray_and_get_normal(&ray, max_dist, true, query_filter(mask));
    let Some((ch, inter)) = hit else { return enc_f64(0.0) };
    let p = ray.point_at(inter.time_of_impact);
    let slot = body_slot_of_collider(wd, ch);
    let s = wd.scratch.data();
    s[0] = p.x as f64;
    s[1] = p.y as f64;
    s[2] = p.z as f64;
    s[3] = inter.normal.x as f64;
    s[4] = inter.normal.y as f64;
    s[5] = inter.normal.z as f64;
    s[6] = inter.time_of_impact as f64;
    s[7] = slot as f64;
    enc_f64(1.0)
}

unsafe extern "sysv64" fn raycast_all(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let origin = arg_vec3(cf, 0);
    let dir = arg_vec3(cf, 3);
    let max_dist = arg_f(cf, 6) as f32;
    let mask = if cf_arg(cf, 7) == UNDEF { u32::MAX } else { arg_u(cf, 7) };
    let len = dir.length();
    if len < 1e-6 || max_dist <= 0.0 {
        return enc_f64(0.0);
    }
    let ray = Ray::new(origin, dir / len);
    let mut hits: Vec<[f32; RAY_STRIDE]> = Vec::new();
    for (ch, _co, inter) in wd.world.intersect_ray(ray, max_dist, true, query_filter(mask)) {
        let p = ray.point_at(inter.time_of_impact);
        let slot = body_slot_of_collider(wd, ch);
        hits.push([
            slot,
            p.x,
            p.y,
            p.z,
            inter.normal.x,
            inter.normal.y,
            inter.normal.z,
            inter.time_of_impact,
        ]);
    }
    hits.sort_by(|a, b| a[7].partial_cmp(&b[7]).unwrap_or(core::cmp::Ordering::Equal));
    let needed = hits.len() * RAY_STRIDE;
    if wd.queries.ensure(g, needed) {
        wd.generation += 1;
    }
    let buf = wd.queries.data();
    for (i, h) in hits.iter().enumerate() {
        buf[i * RAY_STRIDE..(i + 1) * RAY_STRIDE].copy_from_slice(h);
    }
    enc_f64(hits.len() as f64)
}

unsafe fn write_overlap_hits(g: *mut c_void, wd: &mut WorldData, slots: &[f32]) -> u64 {
    if wd.queries.ensure(g, slots.len()) {
        wd.generation += 1;
    }
    wd.queries.data()[..slots.len()].copy_from_slice(slots);
    enc_f64(slots.len() as f64)
}

fn collect_shape_overlaps(wd: &WorldData, pos: Vector, shape: &dyn Shape, mask: u32) -> Vec<f32> {
    let mut slots: Vec<f32> = Vec::new();
    for (ch, _co) in wd.world.intersect_shape(
        Pose::from_parts(pos, Rotation::IDENTITY),
        shape,
        query_filter(mask),
    ) {
        let slot = body_slot_of_collider(wd, ch);
        if slot >= 0.0 && !slots.contains(&slot) {
            slots.push(slot);
        }
    }
    slots
}

unsafe extern "sysv64" fn overlap_sphere(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let pos = arg_vec3(cf, 0);
    let r = (arg_f(cf, 3) as f32).max(1e-4);
    let mask = if cf_arg(cf, 4) == UNDEF { u32::MAX } else { arg_u(cf, 4) };
    let ball = Ball::new(r);
    let slots = collect_shape_overlaps(wd, pos, &ball, mask);
    write_overlap_hits(g, wd, &slots)
}

unsafe extern "sysv64" fn overlap_box(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let pos = arg_vec3(cf, 0);
    let half = Vector::new(
        (arg_f(cf, 3) as f32).max(1e-4),
        (arg_f(cf, 4) as f32).max(1e-4),
        (arg_f(cf, 5) as f32).max(1e-4),
    );
    let mask = if cf_arg(cf, 6) == UNDEF { u32::MAX } else { arg_u(cf, 6) };
    let cuboid = Cuboid::new(half);
    let slots = collect_shape_overlaps(wd, pos, &cuboid, mask);
    write_overlap_hits(g, wd, &slots)
}

unsafe extern "sysv64" fn overlap_point(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let pos = arg_vec3(cf, 0);
    let mask = if cf_arg(cf, 3) == UNDEF { u32::MAX } else { arg_u(cf, 3) };
    let mut slots: Vec<f32> = Vec::new();
    for (ch, _co) in wd.world.intersect_point(pos, query_filter(mask)) {
        let slot = body_slot_of_collider(wd, ch);
        if slot >= 0.0 && !slots.contains(&slot) {
            slots.push(slot);
        }
    }
    write_overlap_hits(g, wd, &slots)
}

// ---- debug render -----------------------------------------------------------

/// Rapier's debug style colors are HSLA ([hue°, sat, light, alpha]) — convert to RGBA.
fn hsla_to_rgba(c: [f32; 4]) -> [f32; 4] {
    let h = c[0].rem_euclid(360.0);
    let (s, l) = (c[1], c[2]);
    let ch = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = ch * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = l - ch / 2.0;
    let (r, g, b) = match (h / 60.0) as u32 {
        0 => (ch, x, 0.0),
        1 => (x, ch, 0.0),
        2 => (0.0, ch, x),
        3 => (0.0, x, ch),
        4 => (x, 0.0, ch),
        _ => (ch, 0.0, x),
    };
    [r + m, g + m, b + m, c[3]]
}

/// debugRender() — snapshot collider wireframes into the queries buffer as
/// per-vertex records (DEBUG_STRIDE, 2 vertices per segment); returns the vertex
/// count. Reuses `queries` like raycastAll — the adapter drains it synchronously.
unsafe extern "sysv64" fn debug_render(g: *mut c_void, cf: *mut c_void) -> u64 {
    use rapier3d::pipeline::{
        DebugRenderBackend, DebugRenderMode, DebugRenderObject, DebugRenderPipeline,
        DebugRenderStyle,
    };

    struct Sink {
        data: Vec<f32>,
    }
    impl DebugRenderBackend for Sink {
        fn draw_line(&mut self, _object: DebugRenderObject, a: Vector, b: Vector, color: [f32; 4]) {
            let c = hsla_to_rgba(color);
            self.data
                .extend_from_slice(&[a.x, a.y, a.z, c[0], c[1], c[2], c[3]]);
            self.data
                .extend_from_slice(&[b.x, b.y, b.z, c[0], c[1], c[2], c[3]]);
        }
    }

    let wd = world!(g, cf);
    let mut sink = Sink { data: Vec::new() };
    // Collider shapes only (no body axes/AABBs, no joints in T1) — parity with
    // the wasm adapter's debugRender view.
    let mut pipeline =
        DebugRenderPipeline::new(DebugRenderStyle::default(), DebugRenderMode::COLLIDER_SHAPES);
    pipeline.render(
        &mut sink,
        &wd.world.bodies,
        &wd.world.colliders,
        &wd.world.impulse_joints,
        &wd.world.multibody_joints,
        &wd.world.narrow_phase,
    );
    if wd.queries.ensure(g, sink.data.len()) {
        wd.generation += 1;
    }
    wd.queries.data()[..sink.data.len()].copy_from_slice(&sink.data);
    enc_f64((sink.data.len() / DEBUG_STRIDE) as f64)
}

// ---- diagnostics ------------------------------------------------------------

unsafe extern "sysv64" fn diagnostics(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let body_count = wd.bodies.iter().count();
    let collider_count = wd.colliders.iter().count();
    let active = wd.world.active_bodies().count();
    let sleeping = wd
        .world
        .rigid_bodies()
        .filter(|(_, b)| b.is_sleeping())
        .count();
    let s = wd.scratch.data();
    s[0] = body_count as f64;
    s[1] = collider_count as f64;
    s[2] = active as f64;
    s[3] = sleeping as f64;
    TRUE
}

// ---- install ----------------------------------------------------------------

pub unsafe fn install(g: *mut c_void) {
    if states().lock().unwrap().contains_key(&(g as usize)) {
        return;
    }
    let cls = make_host_class::<WorldData>("RedPhysics3D");
    let class = define_class(
        g,
        &ClassSpec {
            name: "RedPhysics3D",
            arity: 0,
            construct: Some(ctor),
            parent_proto: 0,
            methods: &[
                ("free", 0, free),
                ("generation", 0, generation),
                ("getPoses", 0, get_poses),
                ("getPosesPrev", 0, get_poses_prev),
                ("getEvents", 0, get_events),
                ("getQueries", 0, get_queries),
                ("getScratch", 0, get_scratch),
                ("setGravity", 3, set_gravity),
                ("setTimestep", 1, set_timestep),
                ("setSolverIterations", 1, set_solver_iterations),
                ("createBody", 0, create_body),
                ("removeBody", 1, remove_body),
                ("addCollider", 3, add_collider),
                ("removeCollider", 1, remove_collider),
                ("setBodyEventsEnabled", 2, set_body_events_enabled),
                ("step", 1, step),
                ("setKinematicTarget", 8, set_kinematic_target),
                ("setBodyPosition", 8, set_body_position),
                ("applyForce", 4, apply_force),
                ("applyImpulse", 4, apply_impulse),
                ("applyTorque", 4, apply_torque),
                ("applyAngularImpulse", 4, apply_angular_impulse),
                ("getLinearVelocity", 1, get_linear_velocity),
                ("setLinearVelocity", 4, set_linear_velocity),
                ("getAngularVelocity", 1, get_angular_velocity),
                ("setAngularVelocity", 4, set_angular_velocity),
                ("setLinearDamping", 2, set_linear_damping),
                ("setAngularDamping", 2, set_angular_damping),
                ("setGravityScale", 2, set_gravity_scale),
                ("getMass", 1, get_mass),
                ("setMass", 2, set_mass),
                ("sleepBody", 1, sleep_body),
                ("wakeBody", 1, wake_body),
                ("isSleeping", 1, is_sleeping),
                ("raycast", 8, raycast),
                ("raycastAll", 8, raycast_all),
                ("overlapSphere", 5, overlap_sphere),
                ("overlapBox", 7, overlap_box),
                ("overlapPoint", 4, overlap_point),
                ("debugRender", 0, debug_render),
                ("diagnostics", 0, diagnostics),
            ],
            getters: &[],
            accessors: &[],
        },
    );
    states()
        .lock()
        .unwrap()
        .insert(g as usize, SendState(ClassState { cls, proto: class.proto }));
}

// ---- tests ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn body_scratch() -> Vec<f64> {
        let mut s = vec![0.0f64; CREATE_BODY_FIELDS];
        s[7] = 1.0; // identity quat w
        s[11] = 1.0; // gravity scale
        s[17] = 1.0; // allowSleep
        s
    }

    fn collider_scratch() -> Vec<f64> {
        let mut s = vec![0.0f64; ADD_COLLIDER_FIELDS];
        s[10] = 1.0; // identity quat w
        s[14] = 1.0; // density
        s[17] = u32::MAX as f64; // layerMask
        s[18] = u32::MAX as f64; // collisionMask
        s
    }

    fn handle(i: u32) -> ColliderHandle {
        ColliderHandle::from_raw_parts(i, 0)
    }

    #[test]
    fn combine_rule_remaps_engine_order() {
        // The whole point: engine Max=2 / Multiply=3 vs rapier Multiply=2 / Max=3.
        assert_eq!(combine_rule(0), CoefficientCombineRule::Average);
        assert_eq!(combine_rule(1), CoefficientCombineRule::Min);
        assert_eq!(combine_rule(2), CoefficientCombineRule::Max);
        assert_eq!(combine_rule(3), CoefficientCombineRule::Multiply);
        assert_eq!(combine_rule(99), CoefficientCombineRule::Average);
    }

    #[test]
    fn constraints_bitfield_maps_to_locked_axes() {
        assert!(locked_axes_from_bits(0).is_empty());
        assert_eq!(locked_axes_from_bits(0b000_001), LockedAxes::TRANSLATION_LOCKED_X);
        assert_eq!(locked_axes_from_bits(0b000_100), LockedAxes::TRANSLATION_LOCKED_Z);
        assert_eq!(locked_axes_from_bits(0b001_000), LockedAxes::ROTATION_LOCKED_X);
        assert_eq!(locked_axes_from_bits(0b111_000), LockedAxes::ROTATION_LOCKED);
        assert_eq!(locked_axes_from_bits(0b111_111), LockedAxes::TRANSLATION_LOCKED | LockedAxes::ROTATION_LOCKED);
    }

    #[test]
    fn quat_from_normalizes_and_degrades_to_identity() {
        let q = quat_from(0.0, 0.0, 0.0, 2.0);
        assert!((q.length() - 1.0).abs() < 1e-5);
        assert!((q.w - 1.0).abs() < 1e-5);
        // Unset authoring data (all zeros) must not poison the pose.
        assert_eq!(quat_from(0.0, 0.0, 0.0, 0.0), Rotation::IDENTITY);
        assert_eq!(quat_from(f64::NAN, 0.0, 0.0, 1.0), Rotation::IDENTITY);
    }

    #[test]
    fn create_body_descriptor_decodes() {
        let mut s = body_scratch();
        s[0] = 0.0; // dynamic
        s[1] = 1.0;
        s[2] = 2.0;
        s[3] = 3.0;
        // 90° about Y.
        let half = std::f32::consts::FRAC_PI_4;
        s[4] = 0.0;
        s[5] = half.sin() as f64;
        s[6] = 0.0;
        s[7] = half.cos() as f64;
        s[9] = 0.25; // linear damping
        s[10] = 0.5; // angular damping
        s[11] = 0.75; // gravity scale
        s[12] = 0b001_010 as f64; // freeze pos Y + rot X
        s[13] = 1.0; // ccd
        s[14] = -5.0; // dominance

        let (builder, body_type, explicit_mass) = body_builder_from_scratch(&s);
        assert_eq!(body_type, 0);
        assert_eq!(explicit_mass, 0.0);

        let mut world = PhysicsWorld::new();
        let h = world.insert_body(builder);
        let rb = world.bodies.get(h).unwrap();
        assert!(rb.is_dynamic());
        assert!((rb.translation() - Vector::new(1.0, 2.0, 3.0)).length() < 1e-5);
        // +X rotated 90° about +Y lands on -Z.
        let fwd = rb.rotation().mul_vec3(Vector::X);
        assert!((fwd - Vector::new(0.0, 0.0, -1.0)).length() < 1e-4, "{fwd:?}");
        assert!((rb.linear_damping() - 0.25).abs() < 1e-6);
        assert!((rb.angular_damping() - 0.5).abs() < 1e-6);
        assert!((rb.gravity_scale() - 0.75).abs() < 1e-6);
        assert_eq!(rb.dominance_group(), -5);
        assert_eq!(
            rb.locked_axes(),
            LockedAxes::TRANSLATION_LOCKED_Y | LockedAxes::ROTATION_LOCKED_X
        );
    }

    #[test]
    fn body_types_map_to_rapier_kinds() {
        let mut world = PhysicsWorld::new();
        for (t, dynamic, kinematic, fixed) in [
            (0.0, true, false, false),
            (1.0, false, true, false),
            (2.0, false, false, true),
        ] {
            let mut s = body_scratch();
            s[0] = t;
            let (b, _, _) = body_builder_from_scratch(&s);
            let h = world.insert_body(b);
            let rb = world.bodies.get(h).unwrap();
            assert_eq!(rb.is_dynamic(), dynamic);
            assert_eq!(rb.is_kinematic(), kinematic);
            assert_eq!(rb.is_fixed(), fixed);
        }
    }

    #[test]
    fn collider_shapes_decode_from_scratch() {
        let mut s = collider_scratch();
        s[0] = SHAPE_BOX as f64;
        s[1] = 1.0;
        s[2] = 2.0;
        s[3] = 3.0;
        let b = collider_builder_from_scratch(&s, vec![], vec![], BODY_STATIC).unwrap();
        let cuboid = b.shape.as_cuboid().expect("cuboid");
        assert_eq!(cuboid.half_extents, Vector::new(1.0, 2.0, 3.0));

        let mut s = collider_scratch();
        s[0] = SHAPE_SPHERE as f64;
        s[1] = 2.5;
        let b = collider_builder_from_scratch(&s, vec![], vec![], BODY_STATIC).unwrap();
        assert_eq!(b.shape.as_ball().expect("ball").radius, 2.5);

        let mut s = collider_scratch();
        s[0] = SHAPE_CAPSULE as f64;
        s[1] = 1.5; // half height
        s[2] = 0.5; // radius
        let b = collider_builder_from_scratch(&s, vec![], vec![], BODY_STATIC).unwrap();
        let cap = b.shape.as_capsule().expect("capsule");
        assert_eq!(cap.radius, 0.5);
        // capsule_y: segment along local Y, half length 1.5.
        assert!((cap.segment.b.y - cap.segment.a.y - 3.0).abs() < 1e-5);
    }

    #[test]
    fn collider_material_and_groups_decode() {
        let mut s = collider_scratch();
        s[0] = SHAPE_SPHERE as f64;
        s[1] = 1.0;
        s[4] = 1.0; // offset x
        s[11] = 1.0; // isTrigger
        s[12] = 0.4; // friction
        s[13] = 0.6; // restitution
        s[14] = 2.0; // density
        s[15] = 2.0; // engine Max
        s[16] = 3.0; // engine Multiply
        s[17] = 0b0110 as f64;
        s[18] = 0b0011 as f64;
        let b = collider_builder_from_scratch(&s, vec![], vec![], 0).unwrap();
        assert!(b.is_sensor);
        assert_eq!(b.friction_combine_rule, CoefficientCombineRule::Max);
        assert_eq!(b.restitution_combine_rule, CoefficientCombineRule::Multiply);
        assert_eq!(b.position.translation, Vector::new(1.0, 0.0, 0.0));
        assert_eq!(b.collision_groups.memberships, Group::from_bits_truncate(0b0110));
        assert_eq!(b.collision_groups.filter, Group::from_bits_truncate(0b0011));
    }

    #[test]
    fn convex_hull_needs_a_real_volume() {
        let mut s = collider_scratch();
        s[0] = SHAPE_CONVEX_HULL as f64;
        let tetra = vec![
            Vector::new(0.0, 0.0, 0.0),
            Vector::new(1.0, 0.0, 0.0),
            Vector::new(0.0, 1.0, 0.0),
            Vector::new(0.0, 0.0, 1.0),
        ];
        assert!(collider_builder_from_scratch(&s, tetra, vec![], 0).is_ok());
        // Two points cannot bound a volume.
        let degenerate = vec![Vector::new(0.0, 0.0, 0.0), Vector::new(1.0, 0.0, 0.0)];
        assert_eq!(
            collider_builder_from_scratch(&s, degenerate, vec![], 0).unwrap_err(),
            ShapeError::DegenerateHull
        );
    }

    #[test]
    fn trimesh_is_static_only_and_index_checked() {
        let mut s = collider_scratch();
        s[0] = SHAPE_TRIMESH as f64;
        let verts = vec![
            Vector::new(0.0, 0.0, 0.0),
            Vector::new(1.0, 0.0, 0.0),
            Vector::new(0.0, 0.0, 1.0),
        ];
        assert!(collider_builder_from_scratch(&s, verts.clone(), vec![[0, 1, 2]], BODY_STATIC).is_ok());
        // Dynamic body: refused loudly instead of shipping a collider-less body.
        assert_eq!(
            collider_builder_from_scratch(&s, verts.clone(), vec![[0, 1, 2]], 0).unwrap_err(),
            ShapeError::TrimeshOnNonFixedBody
        );
        // Kinematic is not "fixed" either.
        assert_eq!(
            collider_builder_from_scratch(&s, verts.clone(), vec![[0, 1, 2]], 1).unwrap_err(),
            ShapeError::TrimeshOnNonFixedBody
        );
        assert_eq!(
            collider_builder_from_scratch(&s, verts.clone(), vec![[0, 1, 9]], BODY_STATIC).unwrap_err(),
            ShapeError::BadTrimesh
        );
        assert_eq!(
            collider_builder_from_scratch(&s, verts, vec![], BODY_STATIC).unwrap_err(),
            ShapeError::BadTrimesh
        );
    }

    #[test]
    fn pose_lane_layout_matches_the_wire_stride() {
        let mut buf = vec![0.0f32; 3 * POSE_STRIDE];
        write_pose(
            &mut buf,
            1,
            Vector::new(1.0, 2.0, 3.0),
            Rotation::from_xyzw(0.1, 0.2, 0.3, 0.9),
        );
        assert_eq!(
            &buf[POSE_STRIDE..2 * POSE_STRIDE],
            &[1.0, 2.0, 3.0, 0.1, 0.2, 0.3, 0.9, 0.0]
        );
        // Out-of-range slot must be a no-op, not a panic.
        write_pose(&mut buf, 99, Vector::ZERO, Rotation::IDENTITY);
    }

    /// A falling body plus a spun body: pose lanes must carry a real 3D translation
    /// AND a live unit quaternion (the 2D path only ever wrote a scalar angle).
    #[test]
    fn step_writes_translation_and_quaternion_lanes() {
        let mut world = PhysicsWorld::new();
        world.gravity = Vector::new(0.0, -9.81, 0.0);
        world.integration_parameters.dt = 1.0 / 60.0;

        let mut s = body_scratch();
        s[2] = 10.0; // y
        let (falling, _, _) = body_builder_from_scratch(&s);
        let falling = world.insert_body(falling.user_data(0));
        world.insert_collider(ColliderBuilder::ball(0.5), Some(falling));

        let mut s = body_scratch();
        s[11] = 0.0; // no gravity — isolate the rotation
        let (spinner, _, _) = body_builder_from_scratch(&s);
        let spinner = world.insert_body(spinner.user_data(1));
        world.insert_collider(ColliderBuilder::cuboid(0.5, 0.5, 0.5), Some(spinner));
        world
            .bodies
            .get_mut(spinner)
            .unwrap()
            .set_angvel(Vector::new(0.0, 4.0, 0.0), true);

        for _ in 0..30 {
            world.step();
        }

        let mut buf = vec![0.0f32; 2 * POSE_STRIDE];
        for (id, h) in [(0u32, falling), (1u32, spinner)] {
            let rb = world.bodies.get(h).unwrap();
            write_pose(&mut buf, id, rb.translation(), *rb.rotation());
        }
        // Falling body: y dropped, x/z untouched.
        assert!(buf[1] < 9.9, "expected the body to fall, y={}", buf[1]);
        assert!(buf[0].abs() < 1e-4 && buf[2].abs() < 1e-4);
        // Spinner: quaternion left identity, stayed normalized, y-axis only.
        let q = Rotation::from_xyzw(
            buf[POSE_STRIDE + 3],
            buf[POSE_STRIDE + 4],
            buf[POSE_STRIDE + 5],
            buf[POSE_STRIDE + 6],
        );
        assert!((q.length() - 1.0).abs() < 1e-3);
        assert!(q.y.abs() > 0.1, "expected rotation about Y, q={q:?}");
        assert!(q.x.abs() < 1e-3 && q.z.abs() < 1e-3);
    }

    /// Contacts reported by rapier carry a world contact point + normal in all
    /// three lanes — a box dropped on the ground reports an up normal.
    #[test]
    fn contact_probe_reports_xyz_point_and_normal() {
        let mut world = PhysicsWorld::new();
        world.gravity = Vector::new(0.0, -9.81, 0.0);
        world.integration_parameters.dt = 1.0 / 60.0;
        let ground = world.insert_body(RigidBodyBuilder::fixed().user_data(0));
        let gc = world.insert_collider(
            ColliderBuilder::cuboid(10.0, 0.5, 10.0).active_events(ActiveEvents::COLLISION_EVENTS),
            Some(ground),
        );
        let falling = world.insert_body(
            RigidBodyBuilder::dynamic()
                .pose(Pose::from_parts(Vector::new(0.0, 2.0, 0.0), Rotation::IDENTITY))
                .user_data(1),
        );
        let fc = world.insert_collider(
            ColliderBuilder::cuboid(0.5, 0.5, 0.5).active_events(ActiveEvents::COLLISION_EVENTS),
            Some(falling),
        );

        let collector = Collector::default();
        for _ in 0..120 {
            world.step_with_events(&(), &collector);
        }
        let pair = world.contact_pair(gc, fc).expect("contact pair");
        let c = pair_contact(&world.colliders, pair);
        // Contact sits on the ground's top face, normal is the world Y axis.
        assert!((c[1] - 0.5).abs() < 0.2, "contact y={}", c[1]);
        assert!(c[4].abs() > 0.9, "normal={:?}", &c[3..6]);
        // Events actually reached the collector.
        assert!(!collector.evs.lock().unwrap().is_empty());
    }

    #[test]
    fn event_fold_enter_then_stay_then_exit() {
        let mut st = EventState::default();
        let mut out: Vec<[f32; EV_STRIDE]> = Vec::new();
        let probe = |_a: ColliderHandle, _b: ColliderHandle| [7.0, 8.0, 9.0, 0.0, 1.0, 0.0];

        st.fold(
            vec![RawEv {
                started: true,
                sensor: false,
                h1: handle(0),
                h2: handle(1),
                a: 3,
                b: 4,
                contact: [1.0, 2.0, 3.0, 0.0, 1.0, 0.0],
            }],
            &mut out,
            &probe,
        );
        // Enter only — the stay for a freshly entered pair starts NEXT step.
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], [EV_COLL_ENTER, 3.0, 4.0, 1.0, 2.0, 3.0, 0.0, 1.0, 0.0]);

        st.fold(vec![], &mut out, &probe);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], [EV_COLL_STAY, 3.0, 4.0, 7.0, 8.0, 9.0, 0.0, 1.0, 0.0]);

        st.fold(
            vec![RawEv {
                started: false,
                sensor: false,
                h1: handle(0),
                h2: handle(1),
                a: 3,
                b: 4,
                contact: [0.0; 6],
            }],
            &mut out,
            &probe,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0][0], EV_COLL_EXIT);
        // Pair is gone: no further stays.
        st.fold(vec![], &mut out, &probe);
        assert!(out.is_empty());
    }

    #[test]
    fn sensor_events_use_the_trigger_kinds() {
        let mut st = EventState::default();
        let mut out: Vec<[f32; EV_STRIDE]> = Vec::new();
        let probe = |_a: ColliderHandle, _b: ColliderHandle| [0.0; 6];
        st.fold(
            vec![RawEv {
                started: true,
                sensor: true,
                h1: handle(2),
                h2: handle(5),
                a: 1,
                b: 2,
                contact: [0.0; 6],
            }],
            &mut out,
            &probe,
        );
        assert_eq!(out[0][0], EV_TRIG_ENTER);
        st.fold(vec![], &mut out, &probe);
        assert_eq!(out[0][0], EV_TRIG_STAY);
    }

    #[test]
    fn events_from_dead_slots_are_dropped() {
        let mut st = EventState::default();
        let mut out: Vec<[f32; EV_STRIDE]> = Vec::new();
        let probe = |_a: ColliderHandle, _b: ColliderHandle| [0.0; 6];
        st.fold(
            vec![RawEv {
                started: true,
                sensor: false,
                h1: handle(0),
                h2: handle(1),
                a: u32::MAX,
                b: 4,
                contact: [0.0; 6],
            }],
            &mut out,
            &probe,
        );
        assert!(out.is_empty());
        assert!(st.active_contacts.is_empty());
    }

    /// Removal-time exits must be flushed FIRST and exactly once: rapier's own
    /// Stopped(REMOVED) lands a step later, when the slot may already be reused.
    #[test]
    fn pending_exits_flush_before_the_next_step_events() {
        let mut st = EventState::default();
        let mut out: Vec<[f32; EV_STRIDE]> = Vec::new();
        let probe = |_a: ColliderHandle, _b: ColliderHandle| [0.0; 6];
        st.fold(
            vec![RawEv {
                started: true,
                sensor: false,
                h1: handle(0),
                h2: handle(1),
                a: 3,
                b: 4,
                contact: [0.0; 6],
            }],
            &mut out,
            &probe,
        );
        st.purge_pairs_involving(|p| p.h1 == handle(0) || p.h2 == handle(0));
        assert_eq!(st.pending_exits.len(), 1);
        assert!(st.active_contacts.is_empty());

        st.fold(
            vec![RawEv {
                started: true,
                sensor: false,
                h1: handle(6),
                h2: handle(7),
                a: 8,
                b: 9,
                contact: [0.0; 6],
            }],
            &mut out,
            &probe,
        );
        assert_eq!(out.len(), 2);
        assert_eq!(out[0][0], EV_COLL_EXIT); // staged exit first
        assert_eq!(out[0][1], 3.0);
        assert_eq!(out[1][0], EV_COLL_ENTER);
        assert!(st.pending_exits.is_empty());
    }

    #[test]
    fn hsla_to_rgba_matches_the_2d_conversion() {
        // Pure red (hue 0, full saturation, mid lightness).
        let c = hsla_to_rgba([0.0, 1.0, 0.5, 1.0]);
        assert!((c[0] - 1.0).abs() < 1e-5 && c[1] < 1e-5 && c[2] < 1e-5 && c[3] == 1.0);
        // Grey (no saturation).
        let c = hsla_to_rgba([210.0, 0.0, 0.25, 0.5]);
        assert!((c[0] - 0.25).abs() < 1e-5 && (c[1] - 0.25).abs() < 1e-5 && (c[2] - 0.25).abs() < 1e-5);
    }
}
