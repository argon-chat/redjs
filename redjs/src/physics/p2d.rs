//! `RedPhysics2D` — the native Rapier2D world exposed to JS as a host class.
//! One JS instance = one physics world. The TS adapter (`@engine/physics2d-native`)
//! implements `IPhysicsEngine2D` over this surface; semantics deliberately mirror
//! the WASM adapter (`packages/physics2d-rapier2d`) so web and native behave the
//! same — zone math, one-way filter and force mapping are 1:1 ports.
//!
//! Protocol (see `.refactor/PHYSICS-NATIVE-DESIGN.md` §3):
//! - descriptors come in through the shared f64 `scratch` buffer (layouts below);
//!   variable-size data (polygon verts) as a Float32Array argument;
//! - `step(dt)` = ONE rapier step (the fixed-timestep accumulator lives in the
//!   TS adapter, matching the WASM adapter) and returns the event-record count;
//! - poses/events/queries are Rust-owned buffers read zero-copy from JS; any
//!   regrow bumps `generation()` and the adapter refetches the views.

use core::ffi::c_void;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use rapier2d::control::{
    CharacterAutostep, CharacterCollision, CharacterLength, KinematicCharacterController,
};
use rapier2d::prelude::*;

use super::{ExposedF32, ExposedF64, Slab};
use crate::capi;
use crate::webapi::util::{
    cf_arg, cf_new_target, cf_this, dec_f64, define_class, enc_bool, enc_f64, get, host_create,
    host_data, is_cell, make_host_class, throw_type_error, ClassSpec, FALSE, TRUE, UNDEF,
};

// ---- wire constants (mirrored in the TS adapter) ---------------------------

/// poses buffer: slot-indexed, stride 4 = [x, y, rot, reserved].
const POSE_STRIDE: usize = 4;
/// events buffer: stride 7 = [kind, bodyA, bodyB, cx, cy, nx, ny].
const EV_STRIDE: usize = 7;
/// event kinds.
const EV_COLL_ENTER: f32 = 0.0;
const EV_COLL_STAY: f32 = 1.0;
const EV_COLL_EXIT: f32 = 2.0;
const EV_TRIG_ENTER: f32 = 3.0;
const EV_TRIG_STAY: f32 = 4.0;
const EV_TRIG_EXIT: f32 = 5.0;
/// raycastAll query records: stride 6 = [bodySlot, px, py, nx, ny, dist].
const RAY_STRIDE: usize = 6;
/// debugRender vertex records (queries buffer): stride 6 = [x, y, r, g, b, a],
/// two vertices per line segment; colors already converted HSLA→RGBA.
const DEBUG_STRIDE: usize = 6;

const SCRATCH_LEN: usize = 512;
const INITIAL_POSES: usize = 1024 * POSE_STRIDE;
const INITIAL_EVENTS: usize = 512 * EV_STRIDE;
const INITIAL_QUERIES: usize = 4096;

// Scratch layouts (f64 indices), kept in sync with the TS adapter:
// createBody:   [0]=bodyType(0 dyn/1 kin/2 static) [1]=x [2]=y [3]=rot
//               [4]=mass(0=auto; >0 = authored TOTAL, re-targeted after colliders)
//               [5]=linearDamping [6]=angularDamping [7]=gravityScale
//               [8]=constraints(bit0 freezeX,bit1 freezeY,bit2 freezeRot)
//               [9]=ccd [10]=dominance [11..12]=sleep thresholds (native-only) [13]=allowSleep
// addCollider:  [0]=shape(0 box/1 circle/2 capsule/3 polygon) [1]=a [2]=b [3]=offX [4]=offY
//               [5]=rot [6]=isTrigger [7]=friction [8]=restitution [9]=density
//               [10]=frictionCombine [11]=restitutionCombine (engine CombineMode values)
//               [12]=layerMask [13]=collisionMask [14]=oneWay [15]=oneWayDirX [16]=oneWayDirY
// gravityZone:  [0]=posX [1]=posY [2]=rot [3]=scaleX [4]=scaleY [5]=shape [6]=type
//               [7]=gravX [8]=gravY [9]=strength [10]=falloff [11]=innerRadius [12]=outerRadius
//               [13]=sizeX [14]=sizeY [15]=direction [16]=priority [17]=affectedLayers
// buoyancyZone: [0]=posX [1]=posY [2]=rot [3]=scaleX [4]=scaleY [5]=density [6]=linearDrag
//               [7]=angularDrag [8]=flowX [9]=flowY [10]=normX [11]=normY [12]=width [13]=height
//               [14]=affectedLayers
// windZone:     [0]=posX [1]=posY [2]=rot [3]=scaleX [4]=scaleY [5]=mode [6]=dirX [7]=dirY
//               [8]=strength [9]=turbulence [10]=turbFreq [11]=pulse [12]=pulseFreq [13]=shape
//               [14]=radius [15]=width [16]=height [17]=falloff [18]=falloffStart [19]=affectedLayers
// surfaceFx:    [0]=speed [1]=speedVariation(unused v1) [2]=forceScale [3]=useContactNormal
//               [4]=useFriction(unused v1) [5]=useBounce(unused v1)
// createJoint:  [0]=type(0 fixed/1 hinge/2 slider/3 rope/4 spring) [1..2]=anchorA [3..4]=anchorB
//               [5]=enableCollision [6]=limitsEnabled [7]=limitMin [8]=limitMax
//               [9]=motorEnabled [10]=motorVel [11]=motorMaxForce [12..13]=axis
//               [14]=maxDistance [15]=restLength [16]=stiffness [17]=damping
// raycast out:  [0]=px [1]=py [2]=nx [3]=ny [4]=dist [5]=bodySlot
// getLinearVelocity out: [0]=vx [1]=vy
// diagnostics out: [0]=bodyCount [1]=colliderCount [2]=activeBodies [3]=sleepingBodies

// ---- world state ------------------------------------------------------------

struct BodyRec {
    handle: RigidBodyHandle,
    /// engine PhysicsBodyType: 0 Dynamic / 1 Kinematic / 2 Static.
    body_type: u8,
    /// last collider material density — buoyancy divisor (WASM-adapter parity).
    density: f32,
    /// Last additional mass we set (rapier REPLACES it on set — needed for drift-free setMass).
    additional_mass: f32,
    /// Authored total mass (>0 = Mass field set; re-targeted after colliders attach).
    explicit_mass: f32,
    /// Union of the body's collider layer memberships (zone affectedLayers filtering; 0 → layer 0).
    layer_bits: u32,
}

struct ColRec {
    handle: ColliderHandle,
    body_id: u32,
}

struct GravityZone {
    pos: Vec2,
    rot: f32,
    scale: Vec2,
    shape: u32,
    kind: u32,
    gravity: Vec2,
    strength: f32,
    falloff: u32,
    inner: f32,
    outer: f32,
    size: Vec2,
    direction: u32,
    priority: f32,
    affected_layers: u32,
    points: Vec<Vec2>,
}

struct BuoyancyZone {
    pos: Vec2,
    rot: f32,
    scale: Vec2,
    density: f32,
    linear_drag: f32,
    angular_drag: f32,
    flow: Vec2,
    normal: Vec2,
    width: f32,
    height: f32,
    affected_layers: u32,
}

struct WindZone {
    pos: Vec2,
    rot: f32,
    scale: Vec2,
    mode: u32,
    dir: Vec2,
    strength: f32,
    turbulence: f32,
    turb_freq: f32,
    pulse: f32,
    pulse_freq: f32,
    shape: u32,
    radius: f32,
    width: f32,
    height: f32,
    falloff: u32,
    falloff_start: f32,
    affected_layers: u32,
}

struct SurfaceFx {
    speed: f32,
    force_scale: f32,
    use_contact_normal: bool,
}

struct CharRec {
    ctl: KinematicCharacterController,
    apply_impulses: bool,
    /// Mass used for pushing dynamics; 0 = the body's own mass.
    mass: f32,
}

/// One-way platform hook. The canonical rapier approach: contact-NORMAL based
/// filtering in `modify_solver_contacts` (a mirror of rapier's own
/// `update_as_oneway_platform` state machine, but on the WORLD normal so the
/// platform may be either collider of the pair and bodies may rotate freely).
/// A position/velocity filter in `filter_contact_pair` cannot express this —
/// the manifold doesn't exist yet at filter time, and the old center-based
/// rule both bonked upward jumpers and let everything through.
#[derive(Default)]
struct Hooks {
    one_way: HashMap<ColliderHandle, Vec2>,
}

/// `ContactModificationContext.user_data` states (per-manifold, persistent).
const OW_UNKNOWN: u32 = 0;
const OW_ALLOWED: u32 = 1;
const OW_FORBIDDEN: u32 = 2;

impl PhysicsHooks for Hooks {
    fn modify_solver_contacts(&self, ctx: &mut ContactModificationContext) {
        let (dir, platform_is_first) = match (
            self.one_way.get(&ctx.collider1),
            self.one_way.get(&ctx.collider2),
        ) {
            (Some(d), _) => (*d, true),
            (_, Some(d)) => (*d, false),
            _ => return,
        };
        // World contact normal points out of collider1 — orient it out of the
        // PLATFORM toward the other body.
        let n_from_platform = if platform_is_first { *ctx.normal } else { -*ctx.normal };
        // 180° arc: contact is "from the solid side" when the platform pushes
        // the other body along the one-way direction.
        let contact_is_ok = n_from_platform.dot(dir) >= 0.0;

        match *ctx.user_data {
            OW_UNKNOWN => {
                if contact_is_ok {
                    *ctx.user_data = OW_ALLOWED;
                } else {
                    ctx.solver_contacts.clear();
                    // Degenerate normals (exact touching) can't classify — wait
                    // for the next frame before latching FORBIDDEN.
                    if n_from_platform.length_squared() > 0.1 {
                        *ctx.user_data = OW_FORBIDDEN;
                    }
                }
            }
            OW_FORBIDDEN => {
                // Keep pass-through until fully separated again, then allow if
                // the approach direction became legal (hysteresis — no popping
                // while overlapping mid-jump).
                if contact_is_ok && ctx.solver_contacts.iter().all(|c| c.dist > 0.0) {
                    *ctx.user_data = OW_ALLOWED;
                } else {
                    ctx.solver_contacts.clear();
                }
            }
            OW_ALLOWED => {
                if ctx.solver_contacts.is_empty() {
                    *ctx.user_data = OW_UNKNOWN;
                }
            }
            _ => *ctx.user_data = OW_UNKNOWN,
        }
    }

    fn filter_intersection_pair(&self, _ctx: &PairFilterContext) -> bool {
        true
    }
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
    contact: [f32; 4],
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
            .unwrap_or([0.0; 4]);
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
fn pair_contact(colliders: &ColliderSet, pair: &ContactPair) -> [f32; 4] {
    if let Some((manifold, contact)) = pair.find_deepest_contact() {
        let n = manifold.data.normal;
        if let Some(co1) = colliders.get(pair.collider1) {
            let p = co1.position().transform_point(contact.local_p1);
            return [p.x, p.y, n.x, n.y];
        }
    }
    [0.0; 4]
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

struct WorldData {
    dead: bool,
    world: PhysicsWorld,
    hooks: Hooks,
    collector: Collector,
    bodies: Slab<BodyRec>,
    colliders: Slab<ColRec>,
    events_enabled: std::collections::HashSet<u32>,
    active_contacts: HashMap<PairKey, ActivePair>,
    active_triggers: HashMap<PairKey, ActivePair>,
    /// Exit records synthesized by removeBody/removeCollider mid-frame (slots
    /// still valid at synthesis time); prepended to the next step's events.
    /// Rapier's own Stopped(REMOVED) for these pairs arrives a step later, when
    /// the slot may already be REUSED by a new body — dispatching then would
    /// name the wrong entity.
    pending_exits: Vec<[f32; EV_STRIDE]>,
    /// Bodies given applyForce/applyTorque since the last step — reset after it
    /// (one-shot semantics; rapier user forces otherwise persist forever).
    forced_bodies: std::collections::HashSet<RigidBodyHandle>,
    gravity_zones: Slab<GravityZone>,
    buoyancy_zones: Slab<BuoyancyZone>,
    wind_zones: Slab<WindZone>,
    surface_fx: HashMap<u32, SurfaceFx>,
    characters: HashMap<u32, CharRec>,
    joints: Slab<ImpulseJointHandle>,
    poses: ExposedF32,
    /// Previous fixed-step poses (same slot layout) — render-rate interpolation
    /// blends prev→curr TS-side using the accumulator remainder.
    prev_poses: ExposedF32,
    events: ExposedF32,
    queries: ExposedF32,
    scratch: ExposedF64,
    generation: u32,
    time: f64,
    ev_staging: Vec<[f32; EV_STRIDE]>,
    dyn_scratch: Vec<(u32, RigidBodyHandle, f32, u32)>,
}

impl WorldData {
    unsafe fn new(g: *mut c_void) -> Self {
        WorldData {
            dead: false,
            world: PhysicsWorld::new(),
            hooks: Hooks::default(),
            collector: Collector::default(),
            bodies: Slab::new(),
            colliders: Slab::new(),
            events_enabled: Default::default(),
            active_contacts: HashMap::new(),
            active_triggers: HashMap::new(),
            pending_exits: Vec::new(),
            forced_bodies: Default::default(),
            gravity_zones: Slab::new(),
            buoyancy_zones: Slab::new(),
            wind_zones: Slab::new(),
            surface_fx: HashMap::new(),
            characters: HashMap::new(),
            joints: Slab::new(),
            poses: ExposedF32::new(g, INITIAL_POSES),
            prev_poses: ExposedF32::new(g, INITIAL_POSES),
            events: ExposedF32::new(g, INITIAL_EVENTS),
            queries: ExposedF32::new(g, INITIAL_QUERIES),
            scratch: ExposedF64::new(g, SCRATCH_LEN),
            generation: 0,
            time: 0.0,
            ev_staging: Vec::new(),
            dyn_scratch: Vec::new(),
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
            return throw_type_error($g, "RedPhysics2D not installed");
        };
        match host_data::<WorldData>($g, cls, cf_this($cf)) {
            Some(w) if !w.dead => w,
            Some(_) => return UNDEF,
            None => return throw_type_error($g, "RedPhysics2D method called on wrong this"),
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

/// Reads a Float32Array argument as Vec2 pairs (copied — safe across JS calls).
unsafe fn arg_verts(g: *mut c_void, cf: *mut c_void, i: u32) -> Vec<Vec2> {
    let v = cf_arg(cf, i);
    if !is_cell(v) {
        return Vec::new();
    }
    let mut ta = crate::ffi::RjsTaInfo::default();
    if crate::glue::rjs_typedarray_info(g, v, &mut ta) != 0 || ta.kind != 8 || ta.data.is_null() {
        return Vec::new();
    }
    let floats = core::slice::from_raw_parts(ta.data as *const f32, ta.length as usize);
    floats.chunks_exact(2).map(|c| Vec2::new(c[0], c[1])).collect()
}

fn combine_rule(v: u32) -> CoefficientCombineRule {
    // engine CombineMode: Average=0 Min=1 Max=2 Multiply=3 → rapier order differs.
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

// ---- lifecycle --------------------------------------------------------------

unsafe extern "sysv64" fn ctor(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some((cls, default_proto)) = class_of(g) else {
        return throw_type_error(g, "RedPhysics2D not installed");
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
        return throw_type_error(g, "RedPhysics2D method called on wrong this");
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
    wd.hooks.one_way.clear();
    wd.events_enabled.clear();
    wd.active_contacts.clear();
    wd.active_triggers.clear();
    wd.pending_exits.clear();
    wd.forced_bodies.clear();
    wd.gravity_zones.clear();
    wd.buoyancy_zones.clear();
    wd.wind_zones.clear();
    wd.surface_fx.clear();
    wd.characters.clear();
    wd.joints.clear();
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
    wd.world.gravity = Vec2::new(arg_f(cf, 0) as f32, arg_f(cf, 1) as f32);
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
/// a full reseed, sleeping bodies would read (0,0,0) until they wake (the
/// active-bodies sync in step() never rewrites them). W-PHYS PHYS-12.
fn reseed_all_poses(wd: &mut WorldData) {
    let poses_len = wd.poses.len();
    wd.dyn_scratch.clear();
    for (id, rec) in wd.bodies.iter() {
        wd.dyn_scratch.push((id, rec.handle, 0.0, 0));
    }
    for i in 0..wd.dyn_scratch.len() {
        let (id, handle, _, _) = wd.dyn_scratch[i];
        let Some(rb) = wd.world.bodies.get(handle) else { continue };
        let base = id as usize * POSE_STRIDE;
        if base + POSE_STRIDE > poses_len {
            continue;
        }
        let t = rb.translation();
        let angle = rb.rotation().angle();
        let buf = wd.poses.data();
        buf[base] = t.x;
        buf[base + 1] = t.y;
        buf[base + 2] = angle;
        let prev = wd.prev_poses.data();
        prev[base] = t.x;
        prev[base + 1] = t.y;
        prev[base + 2] = angle;
    }
}

/// Removes active contact/trigger pairs matching `involves`, staging exit
/// records with the still-valid slot ids. Called at removal time — waiting for
/// rapier's Stopped(REMOVED) a step later risks the slot being reused by a new
/// body and the exit naming the wrong entity. W-PHYS PHYS-12.
fn purge_pairs_involving(wd: &mut WorldData, involves: impl Fn(&ActivePair) -> bool) {
    let keys: Vec<PairKey> = wd
        .active_contacts
        .iter()
        .filter(|(_, p)| involves(p))
        .map(|(k, _)| *k)
        .collect();
    for k in keys {
        if let Some(p) = wd.active_contacts.remove(&k) {
            wd.pending_exits
                .push([EV_COLL_EXIT, p.a as f32, p.b as f32, 0.0, 0.0, 0.0, 0.0]);
        }
    }
    let keys: Vec<PairKey> = wd
        .active_triggers
        .iter()
        .filter(|(_, p)| involves(p))
        .map(|(k, _)| *k)
        .collect();
    for k in keys {
        if let Some(p) = wd.active_triggers.remove(&k) {
            wd.pending_exits
                .push([EV_TRIG_EXIT, p.a as f32, p.b as f32, 0.0, 0.0, 0.0, 0.0]);
        }
    }
}

unsafe extern "sysv64" fn create_body(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let s: Vec<f64> = wd.scratch.data()[..14].to_vec();
    let body_type = s[0] as u8;
    let builder = match body_type {
        1 => RigidBodyBuilder::kinematic_position_based(),
        2 => RigidBodyBuilder::fixed(),
        _ => RigidBodyBuilder::dynamic(),
    };
    let mut builder = builder
        .translation(Vec2::new(s[1] as f32, s[2] as f32))
        .rotation(s[3] as f32)
        .linear_damping(s[5] as f32)
        .angular_damping(s[6] as f32)
        .gravity_scale(s[7] as f32)
        .ccd_enabled(s[9] != 0.0)
        .dominance_group((s[10] as i32).clamp(-127, 127) as i8)
        .can_sleep(s[13] != 0.0);
    let explicit_mass = if s[4] > 0.0 { s[4] as f32 } else { 0.0 };
    if explicit_mass > 0.0 {
        builder = builder.additional_mass(explicit_mass);
    }
    let constraints = s[8] as u32;
    let mut locked = LockedAxes::empty();
    if constraints & 1 != 0 {
        locked |= LockedAxes::TRANSLATION_LOCKED_X;
    }
    if constraints & 2 != 0 {
        locked |= LockedAxes::TRANSLATION_LOCKED_Y;
    }
    if constraints & 4 != 0 {
        locked |= LockedAxes::ROTATION_LOCKED;
    }
    if !locked.is_empty() {
        builder = builder.locked_axes(locked);
    }
    let id = wd.bodies.insert(BodyRec {
        handle: RigidBodyHandle::invalid(),
        body_type,
        density: 1.0,
        additional_mass: explicit_mass,
        explicit_mass,
        layer_bits: 0,
    });
    let handle = wd.world.insert_body(builder.user_data(id as u128));
    wd.bodies.get_mut(id).unwrap().handle = handle;
    // Sleep thresholds (native-only knobs — rapier.js does not expose these).
    if s[11] > 0.0 || s[12] > 0.0 {
        if let Some(rb) = wd.world.bodies.get_mut(handle) {
            let act = rb.activation_mut();
            if s[11] > 0.0 {
                act.normalized_linear_threshold = s[11] as f32;
            }
            if s[12] > 0.0 {
                act.angular_threshold = s[12] as f32;
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
    let base = id as usize * POSE_STRIDE;
    let p = wd.poses.data();
    p[base] = s[1] as f32;
    p[base + 1] = s[2] as f32;
    p[base + 2] = s[3] as f32;
    p[base + 3] = 0.0;
    let prev = wd.prev_poses.data();
    prev[base] = s[1] as f32;
    prev[base + 1] = s[2] as f32;
    prev[base + 2] = s[3] as f32;
    prev[base + 3] = 0.0;
    enc_f64(id as f64)
}

unsafe extern "sysv64" fn remove_body(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let id = arg_u(cf, 0);
    let Some(rec) = wd.bodies.remove(id) else { return UNDEF };
    // Remove collider slots + one-way entries owned by this body (rapier
    // removes the actual colliders together with the body).
    let owned: Vec<(u32, ColliderHandle)> = wd
        .colliders
        .iter()
        .filter(|(_, c)| c.body_id == id)
        .map(|(cid, c)| (cid, c.handle))
        .collect();
    // Stage exits for pairs this body is touching BEFORE its slot can be reused.
    purge_pairs_involving(wd, |p| owned.iter().any(|(_, h)| *h == p.h1 || *h == p.h2));
    for (cid, handle) in owned {
        wd.colliders.remove(cid);
        wd.hooks.one_way.remove(&handle);
    }
    wd.surface_fx.remove(&id);
    wd.characters.remove(&id);
    wd.events_enabled.remove(&id);
    wd.forced_bodies.remove(&rec.handle);
    wd.world.remove_body(rec.handle);
    UNDEF
}

// ---- colliders --------------------------------------------------------------

unsafe extern "sysv64" fn add_collider(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let body_id = arg_u(cf, 0);
    let Some(body) = wd.bodies.get(body_id).map(|r| r.handle) else {
        return enc_f64(-1.0);
    };
    let s: Vec<f64> = wd.scratch.data()[..17].to_vec();
    let shape = s[0] as u32;
    let builder = match shape {
        1 => Some(ColliderBuilder::ball((s[1] as f32).max(1e-4))),
        2 => Some(ColliderBuilder::capsule_y((s[1] as f32).max(0.0), (s[2] as f32).max(1e-4))),
        3 => {
            let verts = arg_verts(g, cf, 1);
            if verts.len() >= 3 {
                ColliderBuilder::convex_hull(&verts)
            } else {
                None
            }
        }
        _ => Some(ColliderBuilder::cuboid((s[1] as f32).max(1e-4), (s[2] as f32).max(1e-4))),
    };
    let Some(builder) = builder else { return enc_f64(-1.0) };
    let is_trigger = s[6] != 0.0;
    let one_way = s[14] != 0.0;
    let mut builder = builder
        .translation(Vec2::new(s[3] as f32, s[4] as f32))
        .rotation(s[5] as f32)
        .sensor(is_trigger)
        .friction(s[7] as f32)
        .restitution(s[8] as f32)
        .density((s[9] as f32).max(1e-4))
        .friction_combine_rule(combine_rule(s[10] as u32))
        .restitution_combine_rule(combine_rule(s[11] as u32))
        .collision_groups(groups(s[12] as u32, s[13] as u32));
    if is_trigger {
        builder = builder.active_events(ActiveEvents::COLLISION_EVENTS);
    }
    if one_way {
        builder = builder.active_hooks(ActiveHooks::MODIFY_SOLVER_CONTACTS);
    }
    let id = wd.colliders.insert(ColRec { handle: ColliderHandle::invalid(), body_id });
    let handle = wd
        .world
        .insert_collider(builder.user_data(id as u128), Some(body));
    wd.colliders.get_mut(id).unwrap().handle = handle;
    if one_way {
        let dir = Vec2::new(s[15] as f32, s[16] as f32);
        let dir = if dir.length_squared() > 1e-6 { dir.normalize() } else { Vec2::new(0.0, 1.0) };
        wd.hooks.one_way.insert(handle, dir);
    }
    if let Some(rec) = wd.bodies.get_mut(body_id) {
        rec.layer_bits |= s[12] as u32;
        if s[9] > 0.0 {
            rec.density = s[9] as f32;
        }
    }
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
    purge_pairs_involving(wd, |p| p.h1 == handle || p.h2 == handle);
    wd.hooks.one_way.remove(&handle);
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
    wd.time += dt as f64;
    wd.world.integration_parameters.dt = dt;

    apply_zone_forces(wd, dt);
    apply_surface_effectors(wd, dt);

    wd.world.step_with_events(&wd.hooks, &wd.collector);

    // One-shot force semantics (builtin/Unity parity, W-PHYS PHYS-13): rapier's
    // add_force/add_torque persist until reset — clear them after the step they
    // were applied to. Mirrors the WASM adapter's per-substep reset.
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
    wd.ev_staging.clear();
    // Exits staged by removeBody/removeCollider since the last step go first
    // (their slots were resolved at removal time, before any reuse).
    for i in 0..wd.pending_exits.len() {
        let rec = wd.pending_exits[i];
        wd.ev_staging.push(rec);
    }
    wd.pending_exits.clear();
    // Pairs that ENTERED this step — their stay starts next step (WASM parity).
    let mut entered: std::collections::HashSet<PairKey> = std::collections::HashSet::new();
    for ev in drained {
        let key = pair_key(ev.h1, ev.h2);
        if ev.started {
            if ev.a == u32::MAX || ev.b == u32::MAX {
                continue;
            }
            let pair = ActivePair { a: ev.a, b: ev.b, h1: ev.h1, h2: ev.h2 };
            let kind = if ev.sensor { EV_TRIG_ENTER } else { EV_COLL_ENTER };
            wd.ev_staging.push([
                kind, ev.a as f32, ev.b as f32, ev.contact[0], ev.contact[1], ev.contact[2],
                ev.contact[3],
            ]);
            entered.insert(key);
            if ev.sensor {
                wd.active_triggers.insert(key, pair);
            } else {
                wd.active_contacts.insert(key, pair);
            }
        } else {
            let map = if ev.sensor { &mut wd.active_triggers } else { &mut wd.active_contacts };
            if let Some(p) = map.remove(&key) {
                let kind = if ev.sensor { EV_TRIG_EXIT } else { EV_COLL_EXIT };
                wd.ev_staging.push([
                    kind, p.a as f32, p.b as f32, ev.contact[0], ev.contact[1], ev.contact[2],
                    ev.contact[3],
                ]);
            }
        }
    }
    // Stay events every step for still-active pairs (WASM-adapter semantics:
    // NOT on the enter step), with a fresh deepest-contact probe for collisions.
    for (key, p) in wd.active_contacts.iter() {
        if entered.contains(key) {
            continue;
        }
        let c = wd
            .world
            .contact_pair(p.h1, p.h2)
            .map(|pair| pair_contact(&wd.world.colliders, pair))
            .unwrap_or([0.0; 4]);
        wd.ev_staging
            .push([EV_COLL_STAY, p.a as f32, p.b as f32, c[0], c[1], c[2], c[3]]);
    }
    for (key, p) in wd.active_triggers.iter() {
        if entered.contains(key) {
            continue;
        }
        wd.ev_staging
            .push([EV_TRIG_STAY, p.a as f32, p.b as f32, 0.0, 0.0, 0.0, 0.0]);
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
        let poses_len = wd.poses.len();
        wd.dyn_scratch.clear();
        for (h, rb) in wd.world.active_bodies() {
            let _ = h;
            let id = rb.user_data as u32;
            let base = id as usize * POSE_STRIDE;
            if base + POSE_STRIDE <= poses_len {
                wd.dyn_scratch.push((id, RigidBodyHandle::invalid(), 0.0, 0));
            }
        }
        // Two passes to appease the borrow checker: collect ids, then write.
        let ids: Vec<u32> = wd.dyn_scratch.iter().map(|(id, _, _, _)| *id).collect();
        for id in ids {
            let Some(handle) = wd.body(id) else { continue };
            let Some(rb) = wd.world.bodies.get(handle) else { continue };
            let t = rb.translation();
            let angle = rb.rotation().angle();
            let base = id as usize * POSE_STRIDE;
            // Shift the outgoing pose into the prev buffer (interpolation pair).
            let (px, py, pr) = {
                let buf = wd.poses.data();
                (buf[base], buf[base + 1], buf[base + 2])
            };
            let prev = wd.prev_poses.data();
            prev[base] = px;
            prev[base + 1] = py;
            prev[base + 2] = pr;
            let buf = wd.poses.data();
            buf[base] = t.x;
            buf[base + 1] = t.y;
            buf[base + 2] = angle;
        }
    }

    enc_f64(wd.ev_staging.len() as f64)
}

// ---- zone forces (1:1 port of the WASM adapter's JS math) -------------------

fn world_to_zone_local(wx: f32, wy: f32, pos: Vec2, rot: f32, scale: Vec2) -> Vec2 {
    let tx = wx - pos.x;
    let ty = wy - pos.y;
    let (sin, cos) = (-rot).sin_cos();
    let rx = tx * cos - ty * sin;
    let ry = tx * sin + ty * cos;
    Vec2::new(
        if scale.x != 0.0 { rx / scale.x } else { rx },
        if scale.y != 0.0 { ry / scale.y } else { ry },
    )
}

struct Containment {
    inside: bool,
    normalized_dist: f32,
    dir: Vec2,
}

fn zone_containment(wx: f32, wy: f32, z: &GravityZone) -> Containment {
    let local = world_to_zone_local(wx, wy, z.pos, z.rot, z.scale);
    let d = z.pos - Vec2::new(wx, wy);
    let dist = d.length();
    let dir = if dist > 0.001 { d / dist } else { Vec2::ZERO };
    let out = |inside: bool, nd: f32| Containment { inside, normalized_dist: nd, dir };
    match z.shape {
        0 => {
            let r = local.length();
            if r > z.outer {
                return out(false, 1.0);
            }
            if r <= z.inner {
                return out(true, 0.0);
            }
            out(true, (r - z.inner) / (z.outer - z.inner))
        }
        1 => {
            let half_w = z.size.x * 0.5;
            let half_h = z.size.y * 0.5;
            if local.x.abs() > half_w || local.y.abs() > half_h {
                return out(false, 1.0);
            }
            let max_inner = half_w.min(half_h);
            let dist_from_edge = (half_w - local.x.abs()).min(half_h - local.y.abs());
            let nd = if max_inner > 0.0 { 1.0 - (dist_from_edge / max_inner).min(1.0) } else { 0.0 };
            out(true, nd)
        }
        2 => {
            let vertical = z.direction == 0;
            let radius = z.outer;
            let half_len = if vertical {
                (z.size.y * 0.5 - radius).max(0.0)
            } else {
                (z.size.x * 0.5 - radius).max(0.0)
            };
            let (main, cross) = if vertical { (local.y, local.x) } else { (local.x, local.y) };
            let d_main = main.abs() - half_len;
            let sdf = if d_main <= 0.0 {
                cross.abs() - radius
            } else {
                (d_main * d_main + cross * cross).sqrt() - radius
            };
            if sdf > 0.0 {
                return out(false, 1.0);
            }
            let nd = if radius > 0.0 { 1.0 - (-sdf / radius).min(1.0) } else { 0.0 };
            out(true, nd)
        }
        3 => {
            let pts = &z.points;
            if pts.len() < 3 {
                return out(false, 1.0);
            }
            let mut inside = false;
            let mut j = pts.len() - 1;
            for i in 0..pts.len() {
                let (pi, pj) = (pts[i], pts[j]);
                if ((pi.y > local.y) != (pj.y > local.y))
                    && (local.x < (pj.x - pi.x) * (local.y - pi.y) / (pj.y - pi.y) + pi.x)
                {
                    inside = !inside;
                }
                j = i;
            }
            if !inside {
                return out(false, 1.0);
            }
            let mut c = Vec2::ZERO;
            for p in pts {
                c += *p;
            }
            c /= pts.len() as f32;
            let mut max_r = 0.0f32;
            for p in pts {
                max_r = max_r.max((*p - c).length());
            }
            let nd = if max_r > 0.0 { ((local - c).length() / max_r).min(1.0) } else { 0.0 };
            out(true, nd)
        }
        _ => out(false, 1.0),
    }
}

fn falloff_weight(nd: f32, falloff: u32) -> f32 {
    if nd <= 0.0 {
        return 1.0;
    }
    if nd >= 1.0 {
        return 0.0;
    }
    match falloff {
        0 => 1.0,
        2 => 1.0 / (1.0 + nd * nd * 3.0),
        3 => 1.0 - (nd * nd * (3.0 - 2.0 * nd)),
        _ => 1.0 - nd,
    }
}

fn effective_gravity(pos: Vec2, global: Vec2, zones: &Slab<GravityZone>, body_bits: u32) -> Vec2 {
    // Separate accumulators (lockstep with the WASM adapter, W-PHYS PHYS-14):
    // Override zones REPLACE base gravity (priority-weighted average among
    // themselves); Additive/Point zones ADD afterwards; Scale multiplies the
    // base only when no override is active.
    let mut g = global;
    let mut override_weight = 0.0f32;
    let mut override_blend = Vec2::ZERO;
    let mut add_blend = Vec2::ZERO;
    let mut scale_accum = 1.0f32;
    for (_, z) in zones.iter() {
        if z.affected_layers & body_bits == 0 {
            continue;
        }
        let c = zone_containment(pos.x, pos.y, z);
        if !c.inside {
            continue;
        }
        let fw = falloff_weight(c.normalized_dist, z.falloff);
        if fw <= 0.0 {
            continue;
        }
        let weight = (z.priority + 1.0) * fw;
        match z.kind {
            0 => {
                override_blend += z.gravity * weight;
                override_weight += weight;
            }
            1 => add_blend += z.gravity * fw,
            2 => add_blend += c.dir * (z.strength * fw),
            3 => scale_accum *= 1.0 + (z.strength - 1.0) * fw,
            _ => {}
        }
    }
    if override_weight > 0.0 {
        g = override_blend / override_weight;
    } else {
        g *= scale_accum;
    }
    g + add_blend
}

fn buoyancy_depth(x: f32, y: f32, z: &BuoyancyZone) -> Option<f32> {
    let local = world_to_zone_local(x, y, z.pos, z.rot, z.scale);
    let half_w = z.width * 0.5;
    let half_h = z.height * 0.5;
    if local.x.abs() > half_w || local.y.abs() > half_h {
        return None;
    }
    let n = z.normal;
    let mag = n.length();
    let n = if mag > 0.001 { n / mag } else { Vec2::new(0.0, 1.0) };
    let projected = -(local.x * n.x + local.y * n.y);
    Some(((projected + half_h) / z.height).clamp(0.0, 1.0))
}

fn wind_falloff(x: f32, y: f32, z: &WindZone) -> Option<f32> {
    let local = world_to_zone_local(x, y, z.pos, z.rot, z.scale);
    let (inside, nd) = if z.shape == 0 {
        let r = local.length();
        (r <= z.radius, if z.radius > 0.0 { r / z.radius } else { 0.0 })
    } else {
        let half_w = z.width * 0.5;
        let half_h = z.height * 0.5;
        let inside = local.x.abs() <= half_w && local.y.abs() <= half_h;
        let fx = if half_w > 0.0 { local.x.abs() / half_w } else { 0.0 };
        let fy = if half_h > 0.0 { local.y.abs() / half_h } else { 0.0 };
        (inside, fx.max(fy))
    };
    if !inside {
        return None;
    }
    let mut factor = 1.0;
    if z.falloff > 0 && nd > z.falloff_start {
        let t = (nd - z.falloff_start) / (1.0 - z.falloff_start);
        factor = if z.falloff == 1 { 1.0 - t } else { 1.0 - (t * t * (3.0 - 2.0 * t)) };
    }
    Some(factor)
}

fn apply_zone_forces(wd: &mut WorldData, dt: f32) {
    let no_zones = wd.gravity_zones.capacity() == 0
        && wd.buoyancy_zones.capacity() == 0
        && wd.wind_zones.capacity() == 0;
    if no_zones {
        return;
    }
    wd.dyn_scratch.clear();
    for (id, rec) in wd.bodies.iter() {
        if rec.body_type == 0 {
            wd.dyn_scratch.push((id, rec.handle, rec.density, rec.layer_bits));
        }
    }
    let global = wd.world.gravity;
    let time = wd.time as f32;
    for i in 0..wd.dyn_scratch.len() {
        let (_, handle, density, layer_bits) = wd.dyn_scratch[i];
        let body_bits = if layer_bits == 0 { 1 } else { layer_bits };
        let Some(rb) = wd.world.bodies.get(handle) else { continue };
        if rb.is_sleeping() {
            continue;
        }
        let pos = rb.translation();
        let gravity_scale = rb.gravity_scale();
        let mass = rb.mass();

        // Gravity zones: Rapier applies global gravity itself — add the delta.
        let mut impulse = Vec2::ZERO;
        if wd.gravity_zones.capacity() > 0 {
            let eff = effective_gravity(pos, global, &wd.gravity_zones, body_bits);
            let delta = (eff - global) * gravity_scale;
            if delta.x.abs() > 0.0001 || delta.y.abs() > 0.0001 {
                impulse += delta * mass * dt;
            }
        }

        // Buoyancy (impulse + velocity drag), Archimedes with body density.
        let mut drag: Option<(f32, f32)> = None;
        for (_, z) in wd.buoyancy_zones.iter() {
            if z.affected_layers & body_bits == 0 {
                continue;
            }
            let Some(depth) = buoyancy_depth(pos.x, pos.y, z) else { continue };
            let n = z.normal;
            let mag = n.length();
            let n = if mag > 0.001 { n / mag } else { Vec2::new(0.0, 1.0) };
            let body_density = if density > 0.001 { density } else { 1.0 };
            let f = (z.density / body_density) * mass * 9.81 * depth;
            impulse += (n * f + z.flow * (depth * mass)) * dt;
            let lin = (1.0 - z.linear_drag * dt * depth).max(0.0);
            let ang = (1.0 - z.angular_drag * dt * depth).max(0.0);
            drag = Some(match drag {
                Some((l, a)) => (l * lin, a * ang),
                None => (lin, ang),
            });
        }

        // Wind.
        for (_, z) in wd.wind_zones.iter() {
            if z.affected_layers & body_bits == 0 {
                continue;
            }
            let Some(factor) = wind_falloff(pos.x, pos.y, z) else { continue };
            let mut strength = z.strength * factor;
            if z.turbulence > 0.0 {
                let noise = (time * z.turb_freq * 6.28 + pos.x * 0.5).sin()
                    * (time * z.turb_freq * 4.17 + pos.y * 0.7).cos();
                strength *= 1.0 + z.turbulence * noise;
            }
            if z.pulse > 0.0 {
                strength *= 1.0 + z.pulse * (time * z.pulse_freq * 6.28).sin();
            }
            let w = if z.mode == 0 {
                let m = z.dir.length();
                if m < 0.001 {
                    continue;
                }
                z.dir / m
            } else {
                let d = pos - z.pos;
                let m = d.length();
                if m < 0.001 {
                    continue;
                }
                d / m
            };
            impulse += w * strength * dt;
        }

        if impulse != Vec2::ZERO || drag.is_some() {
            if let Some(rb) = wd.world.bodies.get_mut(handle) {
                if impulse != Vec2::ZERO {
                    rb.apply_impulse(impulse, true);
                }
                if let Some((lin, ang)) = drag {
                    let lv = rb.linvel();
                    rb.set_linvel(lv * lin, true);
                    let av = rb.angvel();
                    rb.set_angvel(av * ang, true);
                }
            }
        }
    }
}

/// Conveyor belts: servo the contact-tangential velocity of touching dynamic
/// bodies toward `speed`. First working implementation in the engine (both the
/// builtin solver and the WASM adapter left this as a TODO).
fn apply_surface_effectors(wd: &mut WorldData, dt: f32) {
    if wd.surface_fx.is_empty() {
        return;
    }
    // (other body handle, impulse)
    let mut impulses: Vec<(RigidBodyHandle, Vec2)> = Vec::new();
    for (&body_id, fx) in wd.surface_fx.iter() {
        let Some(rec) = wd.bodies.get(body_id) else { continue };
        let plat_handle = rec.handle;
        let plat_vel = wd
            .world
            .bodies
            .get(plat_handle)
            .map(|b| b.linvel())
            .unwrap_or(Vec2::ZERO);
        let owned: Vec<ColliderHandle> = wd
            .colliders
            .iter()
            .filter(|(_, c)| c.body_id == body_id)
            .map(|(_, c)| c.handle)
            .collect();
        for ch in owned {
            for pair in wd.world.contact_pairs_with(ch) {
                if !pair.has_any_active_contact() {
                    continue;
                }
                let Some((manifold, _)) = pair.find_deepest_contact() else { continue };
                // World normal oriented away from the effector's collider.
                let mut n = manifold.data.normal;
                let other_ch = if pair.collider1 == ch { pair.collider2 } else { pair.collider1 };
                if pair.collider1 != ch {
                    n = -n;
                }
                let Some(other_bh) = wd.world.colliders.get(other_ch).and_then(|c| c.parent())
                else {
                    continue;
                };
                let Some(other) = wd.world.bodies.get(other_bh) else { continue };
                if !other.is_dynamic() {
                    continue;
                }
                // Tangent: n rotated -90° → for an up normal, +speed pushes +X.
                let t = if fx.use_contact_normal {
                    Vec2::new(n.y, -n.x)
                } else {
                    Vec2::new(1.0, 0.0)
                };
                let rel = other.linvel() - plat_vel;
                let v_t = rel.dot(t);
                let delta = fx.speed - v_t;
                let imp = t * (delta * fx.force_scale * other.mass() * dt);
                impulses.push((other_bh, imp));
            }
        }
    }
    for (h, imp) in impulses {
        if let Some(rb) = wd.world.bodies.get_mut(h) {
            rb.apply_impulse(imp, true);
        }
    }
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

unsafe extern "sysv64" fn apply_force(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let id = arg_u(cf, 0);
    let Some(handle) = wd.body(id) else { return UNDEF };
    let Some(rb) = wd.world.bodies.get_mut(handle) else { return UNDEF };
    rb.add_force(Vec2::new(arg_f(cf, 1) as f32, arg_f(cf, 2) as f32), true);
    wd.forced_bodies.insert(handle); // one-shot: reset after the next step
    UNDEF
}

unsafe extern "sysv64" fn apply_impulse(g: *mut c_void, cf: *mut c_void) -> u64 {
    with_body_mut!(g, cf, wd, rb, {
        rb.apply_impulse(Vec2::new(arg_f(cf, 1) as f32, arg_f(cf, 2) as f32), true);
        UNDEF
    })
}

unsafe extern "sysv64" fn apply_torque(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let id = arg_u(cf, 0);
    let Some(handle) = wd.body(id) else { return UNDEF };
    let Some(rb) = wd.world.bodies.get_mut(handle) else { return UNDEF };
    rb.add_torque(arg_f(cf, 1) as f32, true);
    wd.forced_bodies.insert(handle); // one-shot: reset after the next step
    UNDEF
}

unsafe extern "sysv64" fn apply_angular_impulse(g: *mut c_void, cf: *mut c_void) -> u64 {
    with_body_mut!(g, cf, wd, rb, {
        rb.apply_torque_impulse(arg_f(cf, 1) as f32, true);
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
    TRUE
}

unsafe extern "sysv64" fn set_linear_velocity(g: *mut c_void, cf: *mut c_void) -> u64 {
    with_body_mut!(g, cf, wd, rb, {
        rb.set_linvel(Vec2::new(arg_f(cf, 1) as f32, arg_f(cf, 2) as f32), true);
        UNDEF
    })
}

unsafe extern "sysv64" fn get_angular_velocity(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let id = arg_u(cf, 0);
    let v = wd
        .body(id)
        .and_then(|h| wd.world.bodies.get(h))
        .map(|rb| rb.angvel())
        .unwrap_or(0.0);
    enc_f64(v as f64)
}

unsafe extern "sysv64" fn set_angular_velocity(g: *mut c_void, cf: *mut c_void) -> u64 {
    with_body_mut!(g, cf, wd, rb, {
        rb.set_angvel(arg_f(cf, 1) as f32, true);
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
    // collider-derived base, not the current total (repeated calls drifted).
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
        rb.set_next_kinematic_position(Pose::new(
            Vec2::new(arg_f(cf, 1) as f32, arg_f(cf, 2) as f32),
            arg_f(cf, 3) as f32,
        ));
        UNDEF
    })
}

/// Teleport (editor/scripts moving bodies directly).
unsafe extern "sysv64" fn set_body_position(g: *mut c_void, cf: *mut c_void) -> u64 {
    with_body_mut!(g, cf, wd, rb, {
        rb.set_position(
            Pose::new(Vec2::new(arg_f(cf, 1) as f32, arg_f(cf, 2) as f32), arg_f(cf, 3) as f32),
            true,
        );
        UNDEF
    })
}

// ---- character controller ---------------------------------------------------

/// configureCharacter(bodyId) — scratch: [0]=offset [1]=slide [2]=autostepEnabled
/// [3]=autostepMaxHeight [4]=autostepMinWidth [5]=autostepIncludeDynamic
/// [6]=maxSlopeClimbAngle(rad) [7]=minSlopeSlideAngle(rad) [8]=snapDistance(0=off)
/// [9]=applyImpulsesToDynamic [10]=characterMass(0=body mass)
unsafe extern "sysv64" fn configure_character(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let body_id = arg_u(cf, 0);
    let s: Vec<f64> = wd.scratch.data()[..11].to_vec();
    let mut ctl = KinematicCharacterController::default();
    ctl.up = Vec2::new(0.0, 1.0);
    ctl.offset = CharacterLength::Absolute((s[0] as f32).max(0.001));
    ctl.slide = s[1] != 0.0;
    ctl.autostep = if s[2] != 0.0 {
        Some(CharacterAutostep {
            max_height: CharacterLength::Absolute(s[3] as f32),
            min_width: CharacterLength::Absolute(s[4] as f32),
            include_dynamic_bodies: s[5] != 0.0,
        })
    } else {
        None
    };
    ctl.max_slope_climb_angle = s[6] as f32;
    ctl.min_slope_slide_angle = s[7] as f32;
    ctl.snap_to_ground = if s[8] > 0.0 {
        Some(CharacterLength::Absolute(s[8] as f32))
    } else {
        None
    };
    wd.characters.insert(
        body_id,
        CharRec { ctl, apply_impulses: s[9] != 0.0, mass: s[10] as f32 },
    );
    UNDEF
}

unsafe extern "sysv64" fn remove_character(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    wd.characters.remove(&arg_u(cf, 0));
    UNDEF
}

/// moveCharacter(bodyId, dx, dy, dt) → scratch [0]=appliedX [1]=appliedY
/// [2]=grounded; returns true when a controller ran.
unsafe extern "sysv64" fn move_character(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let body_id = arg_u(cf, 0);
    let desired = Vec2::new(arg_f(cf, 1) as f32, arg_f(cf, 2) as f32);
    let dt = (arg_f(cf, 3) as f32).max(1e-6);

    let Some(body_handle) = wd.body(body_id) else { return FALSE };
    let Some(ch) = wd.characters.get(&body_id) else { return FALSE };
    let ctl = ch.ctl;
    let apply_impulses = ch.apply_impulses;
    let cfg_mass = ch.mass;
    // First collider carries the character's shape.
    let Some(col_handle) = wd
        .colliders
        .iter()
        .find(|(_, c)| c.body_id == body_id)
        .map(|(_, c)| c.handle)
    else {
        return FALSE;
    };
    // Clone the SharedShape (Arc) so the impulse phase can alias the collider
    // set mutably without holding a borrow into it.
    let (shape, shape_pos) = {
        let Some(co) = wd.world.colliders.get(col_handle) else { return FALSE };
        (co.shared_shape().clone(), *co.position())
    };

    let filter = QueryFilter {
        exclude_rigid_body: Some(body_handle),
        ..QueryFilter::default()
    };
    let mut collisions: Vec<CharacterCollision> = Vec::new();
    let eff = {
        let queries = wd.world.broad_phase.as_query_pipeline(
            wd.world.narrow_phase.query_dispatcher(),
            &wd.world.bodies,
            &wd.world.colliders,
            filter,
        );
        ctl.move_shape(dt, &queries, &*shape, &shape_pos, desired, |c| collisions.push(c))
    };

    // Apply as the body's next-kinematic target (contract: kinematic bodies).
    if let Some(rb) = wd.world.bodies.get_mut(body_handle) {
        if rb.is_kinematic() {
            let target = rb.translation() + eff.translation;
            rb.set_next_kinematic_translation(target);
        }
    }

    // Push dynamic bodies we ran into (rapier's approximate impulse solver).
    if apply_impulses && !collisions.is_empty() {
        let mass = if cfg_mass > 0.0 {
            cfg_mass
        } else {
            wd.world.bodies.get(body_handle).map(|b| b.mass()).unwrap_or(1.0).max(0.001)
        };
        let filter = QueryFilter {
            exclude_rigid_body: Some(body_handle),
            ..QueryFilter::default()
        };
        let mut queries = wd.world.broad_phase.as_query_pipeline_mut(
            wd.world.narrow_phase.query_dispatcher(),
            &mut wd.world.bodies,
            &mut wd.world.colliders,
            filter,
        );
        ctl.solve_character_collision_impulses(dt, &mut queries, &*shape, mass, collisions.iter());
    }

    let s = wd.scratch.data();
    s[0] = eff.translation.x as f64;
    s[1] = eff.translation.y as f64;
    s[2] = if eff.grounded { 1.0 } else { 0.0 };
    TRUE
}

// ---- queries ----------------------------------------------------------------

unsafe fn body_slot_of_collider(wd: &WorldData, ch: ColliderHandle) -> f32 {
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
    let origin = Vec2::new(arg_f(cf, 0) as f32, arg_f(cf, 1) as f32);
    let dir = Vec2::new(arg_f(cf, 2) as f32, arg_f(cf, 3) as f32);
    let max_dist = arg_f(cf, 4) as f32;
    let mask = if cf_arg(cf, 5) == UNDEF { u32::MAX } else { arg_u(cf, 5) };
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
    s[2] = inter.normal.x as f64;
    s[3] = inter.normal.y as f64;
    s[4] = inter.time_of_impact as f64;
    s[5] = slot as f64;
    enc_f64(1.0)
}

unsafe extern "sysv64" fn raycast_all(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let origin = Vec2::new(arg_f(cf, 0) as f32, arg_f(cf, 1) as f32);
    let dir = Vec2::new(arg_f(cf, 2) as f32, arg_f(cf, 3) as f32);
    let max_dist = arg_f(cf, 4) as f32;
    let mask = if cf_arg(cf, 5) == UNDEF { u32::MAX } else { arg_u(cf, 5) };
    let len = dir.length();
    if len < 1e-6 || max_dist <= 0.0 {
        return enc_f64(0.0);
    }
    let ray = Ray::new(origin, dir / len);
    let mut hits: Vec<[f32; RAY_STRIDE]> = Vec::new();
    for (ch, _co, inter) in wd.world.intersect_ray(ray, max_dist, true, query_filter(mask)) {
        let p = ray.point_at(inter.time_of_impact);
        let slot = body_slot_of_collider(wd, ch);
        hits.push([slot, p.x, p.y, inter.normal.x, inter.normal.y, inter.time_of_impact]);
    }
    hits.sort_by(|a, b| a[5].partial_cmp(&b[5]).unwrap_or(core::cmp::Ordering::Equal));
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

unsafe fn collect_shape_overlaps(
    wd: &WorldData,
    pos: Vec2,
    shape: &dyn Shape,
    mask: u32,
) -> Vec<f32> {
    let mut slots: Vec<f32> = Vec::new();
    for (ch, _co) in wd.world.intersect_shape(Pose::new(pos, 0.0), shape, query_filter(mask)) {
        let slot = body_slot_of_collider(wd, ch);
        if slot >= 0.0 && !slots.contains(&slot) {
            slots.push(slot);
        }
    }
    slots
}

unsafe extern "sysv64" fn overlap_circle(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let pos = Vec2::new(arg_f(cf, 0) as f32, arg_f(cf, 1) as f32);
    let r = (arg_f(cf, 2) as f32).max(1e-4);
    let mask = if cf_arg(cf, 3) == UNDEF { u32::MAX } else { arg_u(cf, 3) };
    let ball = Ball::new(r);
    let slots = collect_shape_overlaps(wd, pos, &ball, mask);
    write_overlap_hits(g, wd, &slots)
}

unsafe extern "sysv64" fn overlap_box(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let pos = Vec2::new(arg_f(cf, 0) as f32, arg_f(cf, 1) as f32);
    let hx = (arg_f(cf, 2) as f32).max(1e-4);
    let hy = (arg_f(cf, 3) as f32).max(1e-4);
    let mask = if cf_arg(cf, 4) == UNDEF { u32::MAX } else { arg_u(cf, 4) };
    let cuboid = Cuboid::new(Vec2::new(hx, hy));
    let slots = collect_shape_overlaps(wd, pos, &cuboid, mask);
    write_overlap_hits(g, wd, &slots)
}

unsafe extern "sysv64" fn overlap_point(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let pos = Vec2::new(arg_f(cf, 0) as f32, arg_f(cf, 1) as f32);
    let mask = if cf_arg(cf, 2) == UNDEF { u32::MAX } else { arg_u(cf, 2) };
    let mut slots: Vec<f32> = Vec::new();
    for (ch, _co) in wd.world.intersect_point(pos, query_filter(mask)) {
        let slot = body_slot_of_collider(wd, ch);
        if slot >= 0.0 && !slots.contains(&slot) {
            slots.push(slot);
        }
    }
    write_overlap_hits(g, wd, &slots)
}

// ---- joints (PHYS-21) -------------------------------------------------------

/// createJoint(bodyA, bodyB) — descriptor from scratch (layout in header). All values
/// arrive in PHYSICS space (anchors pre-scaled body-local, angles negated engine-side).
/// Returns the joint slot id, or -1 when a body slot is dead.
unsafe extern "sysv64" fn create_joint(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let (Some(ha), Some(hb)) = (wd.body(arg_u(cf, 0)), wd.body(arg_u(cf, 1))) else {
        return enc_f64(-1.0);
    };
    let s: Vec<f64> = wd.scratch.data()[..18].to_vec();
    let a1 = Vec2::new(s[1] as f32, s[2] as f32);
    let a2 = Vec2::new(s[3] as f32, s[4] as f32);
    let contacts = s[5] != 0.0;
    let joint: GenericJoint = match s[0] as u32 {
        1 => {
            let mut b = RevoluteJointBuilder::new()
                .local_anchor1(a1)
                .local_anchor2(a2)
                .contacts_enabled(contacts);
            if s[6] != 0.0 {
                b = b.limits([s[7] as f32, s[8] as f32]);
            }
            if s[9] != 0.0 {
                b = b.motor_velocity(s[10] as f32, 1.0).motor_max_force(s[11] as f32);
            }
            b.into()
        }
        2 => {
            let ax = Vec2::new(s[12] as f32, s[13] as f32);
            let ax = if ax.length() > 1e-6 { ax.normalize() } else { Vec2::new(1.0, 0.0) };
            let mut b = PrismaticJointBuilder::new(ax)
                .local_anchor1(a1)
                .local_anchor2(a2)
                .contacts_enabled(contacts);
            if s[6] != 0.0 {
                b = b.limits([s[7] as f32, s[8] as f32]);
            }
            if s[9] != 0.0 {
                b = b.motor_velocity(s[10] as f32, 1.0).motor_max_force(s[11] as f32);
            }
            b.into()
        }
        3 => RopeJointBuilder::new((s[14] as f32).max(1e-4))
            .local_anchor1(a1)
            .local_anchor2(a2)
            .contacts_enabled(contacts)
            .into(),
        4 => SpringJointBuilder::new(s[15] as f32, s[16] as f32, s[17] as f32)
            .local_anchor1(a1)
            .local_anchor2(a2)
            .contacts_enabled(contacts)
            .into(),
        _ => FixedJointBuilder::new()
            .local_anchor1(a1)
            .local_anchor2(a2)
            .contacts_enabled(contacts)
            .into(),
    };
    let jh = wd.world.insert_impulse_joint(ha, hb, joint);
    enc_f64(wd.joints.insert(jh) as f64)
}

/// removeJoint(slot) — no-op for dead slots (rapier already killed joints whose body
/// was removed; the stale ImpulseJointHandle makes remove_impulse_joint return None).
unsafe extern "sysv64" fn remove_joint(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    if let Some(jh) = wd.joints.remove(arg_u(cf, 0)) {
        wd.world.remove_impulse_joint(jh);
    }
    UNDEF
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

/// debugRender() — snapshot collider/joint wireframes into the queries buffer as
/// per-vertex records (DEBUG_STRIDE, 2 vertices per segment); returns the vertex
/// count. Reuses `queries` like raycastAll — the adapter drains it synchronously.
unsafe extern "sysv64" fn debug_render(g: *mut c_void, cf: *mut c_void) -> u64 {
    use rapier2d::pipeline::{
        DebugRenderBackend, DebugRenderMode, DebugRenderObject, DebugRenderPipeline,
        DebugRenderStyle,
    };

    struct Sink {
        data: Vec<f32>,
    }
    impl DebugRenderBackend for Sink {
        fn draw_line(&mut self, _object: DebugRenderObject, a: Vector, b: Vector, color: [f32; 4]) {
            let c = hsla_to_rgba(color);
            self.data.extend_from_slice(&[a.x, a.y, c[0], c[1], c[2], c[3]]);
            self.data.extend_from_slice(&[b.x, b.y, c[0], c[1], c[2], c[3]]);
        }
    }

    let wd = world!(g, cf);
    let mut sink = Sink { data: Vec::new() };
    // Shapes + joints (no body axes/AABBs) — parity with the wasm adapter's debugRender view.
    let mut pipeline = DebugRenderPipeline::new(
        DebugRenderStyle::default(),
        DebugRenderMode::COLLIDER_SHAPES | DebugRenderMode::JOINTS,
    );
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

// ---- zones CRUD -------------------------------------------------------------

unsafe fn read_gravity_zone(wd: &mut WorldData, g: *mut c_void, cf: *mut c_void, points_arg: u32) -> GravityZone {
    let s: Vec<f64> = wd.scratch.data()[..18].to_vec();
    GravityZone {
        pos: Vec2::new(s[0] as f32, s[1] as f32),
        rot: s[2] as f32,
        scale: Vec2::new(s[3] as f32, s[4] as f32),
        shape: s[5] as u32,
        kind: s[6] as u32,
        gravity: Vec2::new(s[7] as f32, s[8] as f32),
        strength: s[9] as f32,
        falloff: s[10] as u32,
        inner: s[11] as f32,
        outer: s[12] as f32,
        size: Vec2::new(s[13] as f32, s[14] as f32),
        direction: s[15] as u32,
        priority: s[16] as f32,
        affected_layers: s[17] as u32,
        points: arg_verts(g, cf, points_arg),
    }
}

unsafe extern "sysv64" fn add_gravity_zone(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let z = read_gravity_zone(wd, g, cf, 0);
    enc_f64(wd.gravity_zones.insert(z) as f64)
}

unsafe extern "sysv64" fn update_gravity_zone(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let id = arg_u(cf, 0);
    let z = read_gravity_zone(wd, g, cf, 1);
    if let Some(slot) = wd.gravity_zones.get_mut(id) {
        *slot = z;
    }
    UNDEF
}

unsafe extern "sysv64" fn remove_gravity_zone(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    wd.gravity_zones.remove(arg_u(cf, 0));
    UNDEF
}

unsafe fn read_buoyancy_zone(wd: &mut WorldData) -> BuoyancyZone {
    let s: Vec<f64> = wd.scratch.data()[..15].to_vec();
    BuoyancyZone {
        pos: Vec2::new(s[0] as f32, s[1] as f32),
        rot: s[2] as f32,
        scale: Vec2::new(s[3] as f32, s[4] as f32),
        density: s[5] as f32,
        linear_drag: s[6] as f32,
        angular_drag: s[7] as f32,
        flow: Vec2::new(s[8] as f32, s[9] as f32),
        normal: Vec2::new(s[10] as f32, s[11] as f32),
        width: s[12] as f32,
        height: s[13] as f32,
        affected_layers: s[14] as u32,
    }
}

unsafe extern "sysv64" fn add_buoyancy_zone(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let z = read_buoyancy_zone(wd);
    enc_f64(wd.buoyancy_zones.insert(z) as f64)
}

unsafe extern "sysv64" fn update_buoyancy_zone(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let id = arg_u(cf, 0);
    let z = read_buoyancy_zone(wd);
    if let Some(slot) = wd.buoyancy_zones.get_mut(id) {
        *slot = z;
    }
    UNDEF
}

unsafe extern "sysv64" fn remove_buoyancy_zone(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    wd.buoyancy_zones.remove(arg_u(cf, 0));
    UNDEF
}

unsafe fn read_wind_zone(wd: &mut WorldData) -> WindZone {
    let s: Vec<f64> = wd.scratch.data()[..20].to_vec();
    WindZone {
        pos: Vec2::new(s[0] as f32, s[1] as f32),
        rot: s[2] as f32,
        scale: Vec2::new(s[3] as f32, s[4] as f32),
        mode: s[5] as u32,
        dir: Vec2::new(s[6] as f32, s[7] as f32),
        strength: s[8] as f32,
        turbulence: s[9] as f32,
        turb_freq: s[10] as f32,
        pulse: s[11] as f32,
        pulse_freq: s[12] as f32,
        shape: s[13] as u32,
        radius: s[14] as f32,
        width: s[15] as f32,
        height: s[16] as f32,
        falloff: s[17] as u32,
        falloff_start: s[18] as f32,
        affected_layers: s[19] as u32,
    }
}

unsafe extern "sysv64" fn add_wind_zone(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let z = read_wind_zone(wd);
    enc_f64(wd.wind_zones.insert(z) as f64)
}

unsafe extern "sysv64" fn update_wind_zone(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let id = arg_u(cf, 0);
    let z = read_wind_zone(wd);
    if let Some(slot) = wd.wind_zones.get_mut(id) {
        *slot = z;
    }
    UNDEF
}

unsafe extern "sysv64" fn remove_wind_zone(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    wd.wind_zones.remove(arg_u(cf, 0));
    UNDEF
}

unsafe extern "sysv64" fn set_surface_effector(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let body_id = arg_u(cf, 0);
    let s: Vec<f64> = wd.scratch.data()[..6].to_vec();
    wd.surface_fx.insert(
        body_id,
        SurfaceFx {
            speed: s[0] as f32,
            force_scale: s[2] as f32,
            use_contact_normal: s[3] != 0.0,
        },
    );
    UNDEF
}

unsafe extern "sysv64" fn remove_surface_effector(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    wd.surface_fx.remove(&arg_u(cf, 0));
    UNDEF
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
    let cls = make_host_class::<WorldData>("RedPhysics2D");
    let class = define_class(
        g,
        &ClassSpec {
            name: "RedPhysics2D",
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
                ("setGravity", 2, set_gravity),
                ("setTimestep", 1, set_timestep),
                ("setSolverIterations", 1, set_solver_iterations),
                ("createBody", 0, create_body),
                ("removeBody", 1, remove_body),
                ("addCollider", 2, add_collider),
                ("removeCollider", 1, remove_collider),
                ("setBodyEventsEnabled", 2, set_body_events_enabled),
                ("step", 1, step),
                ("applyForce", 3, apply_force),
                ("applyImpulse", 3, apply_impulse),
                ("applyTorque", 2, apply_torque),
                ("applyAngularImpulse", 2, apply_angular_impulse),
                ("getLinearVelocity", 1, get_linear_velocity),
                ("setLinearVelocity", 3, set_linear_velocity),
                ("getAngularVelocity", 1, get_angular_velocity),
                ("setAngularVelocity", 2, set_angular_velocity),
                ("setLinearDamping", 2, set_linear_damping),
                ("setAngularDamping", 2, set_angular_damping),
                ("setGravityScale", 2, set_gravity_scale),
                ("getMass", 1, get_mass),
                ("setMass", 2, set_mass),
                ("sleepBody", 1, sleep_body),
                ("wakeBody", 1, wake_body),
                ("isSleeping", 1, is_sleeping),
                ("setKinematicTarget", 4, set_kinematic_target),
                ("setBodyPosition", 4, set_body_position),
                ("configureCharacter", 1, configure_character),
                ("removeCharacter", 1, remove_character),
                ("moveCharacter", 4, move_character),
                ("raycast", 6, raycast),
                ("raycastAll", 6, raycast_all),
                ("overlapCircle", 4, overlap_circle),
                ("overlapBox", 5, overlap_box),
                ("overlapPoint", 3, overlap_point),
                ("createJoint", 2, create_joint),
                ("removeJoint", 1, remove_joint),
                ("debugRender", 0, debug_render),
                ("addGravityZone", 1, add_gravity_zone),
                ("updateGravityZone", 2, update_gravity_zone),
                ("removeGravityZone", 1, remove_gravity_zone),
                ("addBuoyancyZone", 0, add_buoyancy_zone),
                ("updateBuoyancyZone", 1, update_buoyancy_zone),
                ("removeBuoyancyZone", 1, remove_buoyancy_zone),
                ("addWindZone", 0, add_wind_zone),
                ("updateWindZone", 1, update_wind_zone),
                ("removeWindZone", 1, remove_wind_zone),
                ("setSurfaceEffector", 1, set_surface_effector),
                ("removeSurfaceEffector", 1, remove_surface_effector),
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
