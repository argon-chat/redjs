//! `RedYoga` — the vendored Yoga 3.2.1 flexbox engine exposed to JS as a host
//! class. One JS instance = one layout WORLD (a slot table + a scratch buffer),
//! deliberately mirroring `RedPhysics2D` rather than exposing a class per node:
//! a class per node would put the boundary back exactly where the wasm adapter
//! has it today. The TS adapter implements `ILayoutNode`
//! (`packages/ui/src/layout/ILayoutEngine.ts`) as a plain object holding a u32
//! slot, so the seam is satisfied unchanged.
//!
//! Protocol (`.refactor/UI-LAYOUT-NATIVE-DESIGN.md` §4a — "design the wire
//! protocol batched"):
//!
//! ```text
//! new RedYoga()                 → world
//!   getScratch() : Float64Array   shared in/out payload, >= 64 f64 slots, zero copy
//!   getGeneration() : number      bumps when a view is regenerated → JS refetches
//!   createNode() : number         u32 slot
//!   freeNode(slot)
//!   insertChild(parent, child, index)
//!   removeChild(parent, child)
//!   applyStyle(slot)              reads STYLE_SLOTS f64s from scratch (layout below)
//!   setMeasureFunc(slot, fn|null) fn(width, widthMode, height, heightMode)
//!   markDirty(slot)
//!   calculateLayout(slot, w, h)
//!   syncNode(slot) : number       1 = node reported a NEW layout (flag cleared, and
//!                                 scratch[0..4] = [left, top, width, height]); 0 = nothing
//!   free()
//! ```
//!
//! What this buys, in crossings: `applyStyle` is ONE crossing instead of 34, and
//! `refreshRects`' per-node `hasNewLayout` + 4 getters + `markLayoutSeen` collapses
//! into one `syncNode`. The wasm adapter's per-field diff
//! (`YogaLayoutEngine.ts:169-197`) stays useful on top of this — it decides whether
//! to make the one call at all — which is why the scratch carries values rather
//! than a changed-mask: the mask lives on the JS side already.
//!
//! **Slot 0 is a valid node id.** [`Slab`] hands out dense ids from 0 and reuses
//! them after `freeNode`, exactly like the physics worlds; there is no reserved
//! sentinel, because the TS side expresses "no node" with a null `ILayoutNode`
//! reference and never needs one. Every slot argument is looked up through
//! `Slab::get`, so an out-of-range **or freed** slot throws a TypeError instead of
//! dereferencing anything.
//!
//! # Scratch style layout (f64 indices, written by JS before `applyStyle`)
//!
//! Dimensions take TWO slots — `value` then `unit` (0 = undefined/reset, 1 = point,
//! 2 = percent, 3 = auto) — because the seam's `Dim` is
//! `number | {pct} | 'auto' | undefined` and the unit has to survive the crossing.
//! `unit` 3 is only honoured where yoga has a `*Auto` setter (width/height/
//! flexBasis/margin); everywhere else it is treated as 0 (reset), which is exactly
//! what `dimNoAuto()` does on the JS side today.
//!
//! ```text
//!  0 display        (0 flex, 1 none)          1 flexDirection (0 col,1 col-rev,2 row,3 row-rev)
//!  2 flexWrap       (0 nowrap,1 wrap,2 wrap-reverse)
//!  3 flexGrow       4 flexShrink
//!  5,6   flexBasis  (value, unit)
//!  7 justifyContent (0 flex-start,1 center,2 flex-end,3 space-between,4 space-around,5 space-evenly)
//!  8 alignItems     9 alignSelf    (0 auto,1 flex-start,2 center,3 flex-end,4 stretch,5 baseline)
//! 10 alignContent   (the same six, PLUS 6 space-between,7 space-around,8 space-evenly — the
//!                    distribution keywords are legal on this slot only, see `align_content_of`)
//! 11 rowGap        12 columnGap
//! 13,14 width      15,16 height
//! 17,18 minWidth   19,20 minHeight   21,22 maxWidth   23,24 maxHeight
//! 25,26 marginTop  27,28 marginRight 29,30 marginBottom 31,32 marginLeft
//! 33,34 paddingTop 35,36 paddingRight 37,38 paddingBottom 39,40 paddingLeft
//! 41 borderWidth
//! 42 positionType  (0 relative, 1 absolute)
//! 43,44 top        45,46 right      47,48 bottom      49,50 left
//! 51 aspectRatio   (NaN = undefined)
//! 52 overflow      (0 visible, 1 hidden, 2 scroll)
//! ```
//!
//! Note that the wire enums are the **adapter's** ordinals, not yoga's, and one of
//! them differs: `positionType` 0/1 means relative/absolute while `YGPositionType`
//! is Static=0/Relative=1/Absolute=2. The `align*` family shares one NUMBERING (it
//! is `YGAlign`'s) but not one decoder — slot 10 accepts three values slots 8/9 do
//! not, see [`align_content_of`]. Everything else happens to line up, and
//! [`apply_style_to`] maps all of them explicitly anyway so a future yoga bump
//! cannot silently shift a value.
//!
//! # Measure callbacks: the re-entrancy contract
//!
//! `setMeasureFunc` is mandatory and it is the only hard part of this port: yoga
//! calls a C function pointer from *inside* `YGNodeCalculateLayout`, and that
//! callback has to re-enter JS to run the MSDF text measurer
//! (`UIDocument.wireTextMeasure`, `packages/ui/src/tree/UIDocument.ts:298-303`).
//! The rules, enforced here:
//!
//! - The JS function is stored per node in a [`NodeCtx`] hung off the yoga node's
//!   own context pointer and pinned with `redjs_protect`, released on
//!   `setMeasureFunc(slot, null)`, `freeNode` and `free()`.
//! - Results come back through **the scratch buffer**: JS writes `scratch[0] = width`,
//!   `scratch[1] = height` and returns. No object is allocated per measure, and no
//!   property lookup happens on the native side. For the duration of a measure call
//!   the scratch belongs to the measure protocol — nothing else may be staged in it
//!   across a `calculateLayout`.
//! - **JS may not touch the tree from inside a measure callback.** Every `RedYoga`
//!   method throws while a `calculateLayout` is on the stack (see [`IN_LAYOUT`]).
//!   Mutating yoga mid-layout corrupts its state, and re-entering a host method
//!   would alias the `&mut WorldData` the outer call already holds. The JS adapter
//!   must therefore cache the `getScratch()` view once, not fetch it per measure.
//! - A callback that throws is not swallowed: the exception is parked on the
//!   [`MeasureBus`], the measure reports 0×0 so layout can finish in a defined
//!   state, and `calculateLayout` re-throws it into the caller.
//!
//! The trampoline is `extern "C"` — the *platform* (MS x64) convention, because
//! yoga calls it — and NOT `sysv64`, which is only the convention JSC host
//! functions use. `YGSize` is an 8-byte POD, returned in RAX under the MS ABI by
//! both clang-cl and rustc.
//!
//! # Why the precondition guards are in Rust
//!
//! Yoga's `fatalWithMessage` does `throw std::logic_error` with exceptions enabled
//! (`cpp/yoga/debug/AssertFatal.cpp`), and a C++ exception unwinding out of an
//! `extern "C"` Rust frame aborts the process — a bug that is a catchable JS error
//! today would become a native crash (design doc §4a, "The one genuine hazard").
//! Rather than a try/catch C shim we make the asserts **unreachable**: the four
//! states yoga aborts on are rejected here with a TypeError before the call —
//! inserting a child that still has an owner, inserting into a node with a measure
//! func, setting a measure func on a node with children, and `markDirty` on a node
//! without one. Layout itself can then only assert on states the tree cannot be in.

use core::cell::Cell;
use core::ffi::c_void;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use crate::capi;
use crate::hostbuf::{ExposedF64, Slab};
use crate::webapi::util::{
    cf_arg, cf_new_target, cf_this, dec_f64, define_class, enc_f64, get, host_create, host_data,
    is_cell, make_host_class, throw_type_error, ClassSpec, UNDEF,
};

// ---- yoga C ABI --------------------------------------------------------------
//
// Declared by hand rather than bindgen-ed: adding `bindgen` would make libclang a
// hard toolchain requirement for the whole crate (design doc §4b), for ~150 lines
// of externs. Signatures verified against `cpp/yoga/YGNode.h`, `YGNodeStyle.h`,
// `YGNodeLayout.h` at the vendored commit (facebook/yoga v3.2.1, 042f5013).
// `YG*` enums are plain C `enum`s (`YGMacros.h:51`), i.e. `int`.

#[repr(C)]
pub struct YGNode {
    _private: [u8; 0],
}

pub type YGNodeRef = *mut YGNode;
pub type YGNodeConstRef = *const YGNode;

/// Yoga's measure result. 8-byte POD → returned in RAX under the Microsoft x64
/// ABI, which is what both clang-cl (yoga's caller) and rustc emit here.
#[repr(C)]
pub struct YGSize {
    pub width: f32,
    pub height: f32,
}

type YGMeasureFunc =
    unsafe extern "C" fn(YGNodeConstRef, f32, i32, f32, i32) -> YGSize;

extern "C" {
    fn YGNodeNew() -> YGNodeRef;
    fn YGNodeFree(node: YGNodeRef);
    fn YGNodeInsertChild(node: YGNodeRef, child: YGNodeRef, index: usize);
    fn YGNodeRemoveChild(node: YGNodeRef, child: YGNodeRef);
    fn YGNodeGetChildCount(node: YGNodeConstRef) -> usize;
    fn YGNodeGetOwner(node: YGNodeRef) -> YGNodeRef;
    fn YGNodeCalculateLayout(
        node: YGNodeRef,
        available_width: f32,
        available_height: f32,
        owner_direction: i32,
    );
    fn YGNodeSetMeasureFunc(node: YGNodeRef, measure_func: Option<YGMeasureFunc>);
    fn YGNodeHasMeasureFunc(node: YGNodeConstRef) -> bool;
    fn YGNodeMarkDirty(node: YGNodeRef);
    fn YGNodeGetHasNewLayout(node: YGNodeConstRef) -> bool;
    fn YGNodeSetHasNewLayout(node: YGNodeRef, has_new_layout: bool);
    fn YGNodeSetContext(node: YGNodeRef, context: *mut c_void);
    fn YGNodeGetContext(node: YGNodeConstRef) -> *mut c_void;

    fn YGNodeLayoutGetLeft(node: YGNodeConstRef) -> f32;
    fn YGNodeLayoutGetTop(node: YGNodeConstRef) -> f32;
    fn YGNodeLayoutGetWidth(node: YGNodeConstRef) -> f32;
    fn YGNodeLayoutGetHeight(node: YGNodeConstRef) -> f32;

    fn YGNodeStyleSetDisplay(node: YGNodeRef, display: i32);
    fn YGNodeStyleSetFlexDirection(node: YGNodeRef, flex_direction: i32);
    fn YGNodeStyleSetFlexWrap(node: YGNodeRef, flex_wrap: i32);
    fn YGNodeStyleSetFlexGrow(node: YGNodeRef, flex_grow: f32);
    fn YGNodeStyleSetFlexShrink(node: YGNodeRef, flex_shrink: f32);
    fn YGNodeStyleSetFlexBasis(node: YGNodeRef, flex_basis: f32);
    fn YGNodeStyleSetFlexBasisPercent(node: YGNodeRef, flex_basis: f32);
    fn YGNodeStyleSetFlexBasisAuto(node: YGNodeRef);
    fn YGNodeStyleSetJustifyContent(node: YGNodeRef, justify_content: i32);
    fn YGNodeStyleSetAlignItems(node: YGNodeRef, align_items: i32);
    fn YGNodeStyleSetAlignSelf(node: YGNodeRef, align_self: i32);
    fn YGNodeStyleSetAlignContent(node: YGNodeRef, align_content: i32);
    fn YGNodeStyleSetGap(node: YGNodeRef, gutter: i32, gap_length: f32);
    fn YGNodeStyleSetWidth(node: YGNodeRef, width: f32);
    fn YGNodeStyleSetWidthPercent(node: YGNodeRef, width: f32);
    fn YGNodeStyleSetWidthAuto(node: YGNodeRef);
    fn YGNodeStyleSetHeight(node: YGNodeRef, height: f32);
    fn YGNodeStyleSetHeightPercent(node: YGNodeRef, height: f32);
    fn YGNodeStyleSetHeightAuto(node: YGNodeRef);
    fn YGNodeStyleSetMinWidth(node: YGNodeRef, min_width: f32);
    fn YGNodeStyleSetMinWidthPercent(node: YGNodeRef, min_width: f32);
    fn YGNodeStyleSetMinHeight(node: YGNodeRef, min_height: f32);
    fn YGNodeStyleSetMinHeightPercent(node: YGNodeRef, min_height: f32);
    fn YGNodeStyleSetMaxWidth(node: YGNodeRef, max_width: f32);
    fn YGNodeStyleSetMaxWidthPercent(node: YGNodeRef, max_width: f32);
    fn YGNodeStyleSetMaxHeight(node: YGNodeRef, max_height: f32);
    fn YGNodeStyleSetMaxHeightPercent(node: YGNodeRef, max_height: f32);
    fn YGNodeStyleSetMargin(node: YGNodeRef, edge: i32, margin: f32);
    fn YGNodeStyleSetMarginPercent(node: YGNodeRef, edge: i32, margin: f32);
    fn YGNodeStyleSetMarginAuto(node: YGNodeRef, edge: i32);
    fn YGNodeStyleSetPadding(node: YGNodeRef, edge: i32, padding: f32);
    fn YGNodeStyleSetPaddingPercent(node: YGNodeRef, edge: i32, padding: f32);
    fn YGNodeStyleSetBorder(node: YGNodeRef, edge: i32, border: f32);
    fn YGNodeStyleSetPositionType(node: YGNodeRef, position_type: i32);
    fn YGNodeStyleSetPosition(node: YGNodeRef, edge: i32, position: f32);
    fn YGNodeStyleSetPositionPercent(node: YGNodeRef, edge: i32, position: f32);
    fn YGNodeStyleSetAspectRatio(node: YGNodeRef, aspect_ratio: f32);
    fn YGNodeStyleSetOverflow(node: YGNodeRef, overflow: i32);
}

// yoga enum ordinals, read out of `cpp/yoga/YGEnums.h` (NOT guessed — several of
// them do not match the wire enums, see the module header).

const YG_ALIGN_AUTO: i32 = 0;
const YG_ALIGN_FLEX_START: i32 = 1;
const YG_ALIGN_CENTER: i32 = 2;
const YG_ALIGN_FLEX_END: i32 = 3;
const YG_ALIGN_STRETCH: i32 = 4;
const YG_ALIGN_BASELINE: i32 = 5;
const YG_ALIGN_SPACE_BETWEEN: i32 = 6;
const YG_ALIGN_SPACE_AROUND: i32 = 7;
const YG_ALIGN_SPACE_EVENLY: i32 = 8;

const YG_DIRECTION_LTR: i32 = 1;

const YG_DISPLAY_FLEX: i32 = 0;
const YG_DISPLAY_NONE: i32 = 1;

const YG_EDGE_LEFT: i32 = 0;
const YG_EDGE_TOP: i32 = 1;
const YG_EDGE_RIGHT: i32 = 2;
const YG_EDGE_BOTTOM: i32 = 3;
const YG_EDGE_ALL: i32 = 8;

const YG_FLEX_DIRECTION_COLUMN: i32 = 0;
const YG_FLEX_DIRECTION_COLUMN_REVERSE: i32 = 1;
const YG_FLEX_DIRECTION_ROW: i32 = 2;
const YG_FLEX_DIRECTION_ROW_REVERSE: i32 = 3;

const YG_GUTTER_COLUMN: i32 = 0;
const YG_GUTTER_ROW: i32 = 1;

const YG_JUSTIFY_FLEX_START: i32 = 0;
const YG_JUSTIFY_CENTER: i32 = 1;
const YG_JUSTIFY_FLEX_END: i32 = 2;
const YG_JUSTIFY_SPACE_BETWEEN: i32 = 3;
const YG_JUSTIFY_SPACE_AROUND: i32 = 4;
const YG_JUSTIFY_SPACE_EVENLY: i32 = 5;

const YG_OVERFLOW_VISIBLE: i32 = 0;
const YG_OVERFLOW_HIDDEN: i32 = 1;
const YG_OVERFLOW_SCROLL: i32 = 2;

// Static=0 — the wire has no static, so 0/1 map onto 1/2 here.
const YG_POSITION_TYPE_RELATIVE: i32 = 1;
const YG_POSITION_TYPE_ABSOLUTE: i32 = 2;

const YG_WRAP_NO_WRAP: i32 = 0;
const YG_WRAP_WRAP: i32 = 1;
const YG_WRAP_WRAP_REVERSE: i32 = 2;

// ---- wire constants (mirrored in the TS adapter) -----------------------------

/// Style payload width. Matches the adapter's 34 setter slots once the 19
/// `Dim`-typed ones are widened to (value, unit) pairs.
pub const STYLE_SLOTS: usize = 53;

/// Scratch length. Fixed on purpose and never regrown: the measure trampoline
/// caches this buffer's data pointer in [`MeasureBus`], and a regrow would
/// reallocate the `Vec` under it. 64 leaves headroom over [`STYLE_SLOTS`] for the
/// measure result (`[0]`,`[1]`) and the `syncNode` rect (`[0..4]`) without ever
/// approaching the limit.
const SCRATCH_LEN: usize = 64;

/// `Dim` unit tags. 3 (auto) is only legal where yoga has a `*Auto` setter;
/// [`set_dim`] / [`set_dim_edge`] fall back to "reset" everywhere else, which is
/// what `dimNoAuto()` does on the JS side.
const UNIT_UNDEFINED: u32 = 0;
const UNIT_POINT: u32 = 1;
const UNIT_PERCENT: u32 = 2;
const UNIT_AUTO: u32 = 3;

/// `rjs_kind` tag for a callable (`cpp/glue.cpp:336`).
const KIND_FUNCTION: u32 = 9;

// ---- world state -------------------------------------------------------------

/// Everything a measure callback needs, in an allocation the callback can reach
/// WITHOUT touching [`WorldData`] — the outer `calculateLayout` still holds a
/// `&mut WorldData` while yoga runs, so the callback must not derive a second one.
struct MeasureBus {
    g: *mut c_void,
    /// The world scratch's data pointer. Valid for the world's lifetime because
    /// the scratch is fixed-size (see [`SCRATCH_LEN`]).
    scratch: *mut f64,
    /// First exception thrown by a measure callback during the current
    /// `calculateLayout` (protected). Re-thrown, and cleared, by that call.
    pending_exc: u64,
}

/// Per-node measure state, hung off the yoga node's own context pointer so the
/// trampoline is O(1) with no lookup and no globals. Allocated lazily — only text
/// leaves ever get one.
struct NodeCtx {
    bus: *mut MeasureBus,
    /// The JS measure function (protected), or `UNDEF`.
    func: u64,
}

#[derive(Clone, Copy)]
struct NodeRec {
    node: YGNodeRef,
    /// `null` until `setMeasureFunc(slot, fn)` is called for the first time.
    ctx: *mut NodeCtx,
}

struct WorldData {
    dead: bool,
    nodes: Slab<NodeRec>,
    scratch: ExposedF64,
    bus: *mut MeasureBus,
    /// Bumped whenever an exposed view is regenerated. The scratch never regrows,
    /// so today this is always 0 — the method exists because the JS adapter must
    /// already be written to refetch, and a batched rect buffer (design doc §4a)
    /// would be the first thing to move it.
    generation: u32,
}

impl WorldData {
    unsafe fn new(g: *mut c_void) -> Self {
        // The pointer survives every later move of `WorldData` (into the struct
        // here, then into `host_create`'s Box) because it points into the
        // ExposedF64's heap `Vec`, not into the struct. Only a regrow would
        // invalidate it — which is why the scratch is fixed-size.
        let mut scratch = ExposedF64::new(g, SCRATCH_LEN);
        let scratch_ptr = scratch.data().as_mut_ptr();
        let bus = Box::into_raw(Box::new(MeasureBus { g, scratch: scratch_ptr, pending_exc: UNDEF }));
        WorldData { dead: false, nodes: Slab::new(), scratch, bus, generation: 0 }
    }

    /// Tears a node's measure state down. `release_js` is false on the GC
    /// finalizer path, where no JS may be touched (the protected function then
    /// leaks by design, exactly as `ExposedF64::drop` leaks its buffer).
    unsafe fn drop_node(&mut self, rec: NodeRec, release_js: bool) {
        if !rec.ctx.is_null() {
            let ctx = Box::from_raw(rec.ctx);
            YGNodeSetMeasureFunc(rec.node, None);
            YGNodeSetContext(rec.node, core::ptr::null_mut());
            if release_js && is_cell(ctx.func) {
                crate::ffi::redjs_unprotect((*self.bus).g, ctx.func);
            }
        }
        YGNodeFree(rec.node);
    }
}

impl Drop for WorldData {
    fn drop(&mut self) {
        // GC-finalizer teardown (`free()` never called). Yoga nodes and the boxes
        // are plain native memory and must be reclaimed here, but nothing may
        // touch JS — see `drop_node`'s `release_js`.
        unsafe {
            let recs: Vec<NodeRec> = self.nodes.iter().map(|(_, r)| *r).collect();
            self.nodes.clear();
            for rec in recs {
                self.drop_node(rec, false);
            }
            if !self.bus.is_null() {
                drop(Box::from_raw(self.bus));
                self.bus = core::ptr::null_mut();
            }
        }
    }
}

// ---- per-context class state -------------------------------------------------

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

thread_local! {
    /// Depth of `YGNodeCalculateLayout` calls on this thread's stack. Non-zero
    /// means a measure callback may be running, and every `RedYoga` method must
    /// refuse: yoga's tree is mid-mutation, and the enclosing call already holds
    /// the world's `&mut`. Checked BEFORE `host_data` hands a second one out, so
    /// the aliasing never happens rather than being merely unused.
    static IN_LAYOUT: Cell<u32> = Cell::new(0);
}

fn in_layout() -> bool {
    IN_LAYOUT.with(|c| c.get() != 0)
}

macro_rules! world {
    ($g:expr, $cf:expr) => {{
        if in_layout() {
            return throw_type_error(
                $g,
                "RedYoga: the layout tree is frozen inside a measure callback",
            );
        }
        let Some((cls, _)) = class_of($g) else {
            return throw_type_error($g, "RedYoga not installed");
        };
        match host_data::<WorldData>($g, cls, cf_this($cf)) {
            Some(w) if !w.dead => w,
            Some(_) => return UNDEF,
            None => return throw_type_error($g, "RedYoga method called on wrong this"),
        }
    }};
}

/// Resolves a slot argument to its yoga node, or throws. Out-of-range and freed
/// slots take the same path — [`Slab`] leaves a `None` behind on remove.
macro_rules! node_of {
    ($g:expr, $wd:expr, $slot:expr) => {
        match $wd.nodes.get($slot) {
            Some(rec) => rec.node,
            None => return throw_type_error($g, "RedYoga: invalid node slot"),
        }
    };
}

// ---- arg helpers -------------------------------------------------------------

unsafe fn arg_f(cf: *mut c_void, i: u32) -> f64 {
    dec_f64(cf_arg(cf, i)).unwrap_or(0.0)
}

unsafe fn arg_u(cf: *mut c_void, i: u32) -> u32 {
    arg_f(cf, i) as u32
}

/// An available-size argument. A missing/non-numeric value means *unconstrained*
/// (`YGUndefined` = NaN), never 0 — 0 would collapse the whole tree instead of
/// sizing it to content.
unsafe fn arg_avail(cf: *mut c_void, i: u32) -> f32 {
    dec_f64(cf_arg(cf, i)).map(|v| v as f32).unwrap_or(f32::NAN)
}

// ---- style decoding ----------------------------------------------------------
//
// Wire ordinal → yoga ordinal. Every mapping is explicit and every unknown value
// falls back to the adapter's default, so a garbage f64 can never be handed to a
// C enum parameter. (`v as u32` saturates in Rust, so negatives and NaN land on 0.)

fn display_of(v: f64) -> i32 {
    if v as u32 == 1 { YG_DISPLAY_NONE } else { YG_DISPLAY_FLEX }
}

fn flex_direction_of(v: f64) -> i32 {
    match v as u32 {
        1 => YG_FLEX_DIRECTION_COLUMN_REVERSE,
        2 => YG_FLEX_DIRECTION_ROW,
        3 => YG_FLEX_DIRECTION_ROW_REVERSE,
        _ => YG_FLEX_DIRECTION_COLUMN,
    }
}

fn wrap_of(v: f64) -> i32 {
    match v as u32 {
        1 => YG_WRAP_WRAP,
        2 => YG_WRAP_WRAP_REVERSE,
        _ => YG_WRAP_NO_WRAP,
    }
}

fn justify_of(v: f64) -> i32 {
    match v as u32 {
        1 => YG_JUSTIFY_CENTER,
        2 => YG_JUSTIFY_FLEX_END,
        3 => YG_JUSTIFY_SPACE_BETWEEN,
        4 => YG_JUSTIFY_SPACE_AROUND,
        5 => YG_JUSTIFY_SPACE_EVENLY,
        _ => YG_JUSTIFY_FLEX_START,
    }
}

fn align_of(v: f64) -> i32 {
    match v as u32 {
        1 => YG_ALIGN_FLEX_START,
        2 => YG_ALIGN_CENTER,
        3 => YG_ALIGN_FLEX_END,
        4 => YG_ALIGN_STRETCH,
        5 => YG_ALIGN_BASELINE,
        _ => YG_ALIGN_AUTO,
    }
}

/// `align-content` (slot 10) — [`align_of`] plus the three line-DISTRIBUTION values.
///
/// A second decoder rather than three more arms on [`align_of`], because the extra values are legal
/// on this slot only: `align-items` / `align-self` place ONE box inside its line, so a keyword about
/// spreading space BETWEEN lines has nothing to act on, and yoga answers it by falling back to
/// `flex-start`. Keeping slots 8/9 clamped to 1..5 makes that unreachable instead of silent.
///
/// This is the third decoder on the wire and the only one no JS gate runs (the conformance suite
/// needs `globalThis.RedYoga`), so it is the one that goes stale quietly. It must stay in step with
/// `ALIGN_CONTENT` in `packages/ui/src/layout/NativeLayoutEngine.ts` — same ordinals, which are
/// `YGAlign`'s own.
fn align_content_of(v: f64) -> i32 {
    match v as u32 {
        6 => YG_ALIGN_SPACE_BETWEEN,
        7 => YG_ALIGN_SPACE_AROUND,
        8 => YG_ALIGN_SPACE_EVENLY,
        _ => align_of(v),
    }
}

fn position_type_of(v: f64) -> i32 {
    if v as u32 == 1 { YG_POSITION_TYPE_ABSOLUTE } else { YG_POSITION_TYPE_RELATIVE }
}

fn overflow_of(v: f64) -> i32 {
    match v as u32 {
        1 => YG_OVERFLOW_HIDDEN,
        2 => YG_OVERFLOW_SCROLL,
        _ => YG_OVERFLOW_VISIBLE,
    }
}

type SetF = unsafe extern "C" fn(YGNodeRef, f32);
type SetAutoF = unsafe extern "C" fn(YGNodeRef);
type SetEdgeF = unsafe extern "C" fn(YGNodeRef, i32, f32);
type SetEdgeAutoF = unsafe extern "C" fn(YGNodeRef, i32);

/// One `(value, unit)` pair → the matching yoga entry point. Reset (unit 0, and
/// unit 3 where `auto` has no setter) is a point setter called with `YGUndefined`,
/// which is what `n.setWidth(undefined)` compiles to in the wasm adapter.
unsafe fn set_dim(n: YGNodeRef, v: f64, unit: f64, point: SetF, percent: SetF, auto: Option<SetAutoF>) {
    match unit as u32 {
        UNIT_UNDEFINED => point(n, f32::NAN),
        UNIT_POINT => point(n, v as f32),
        UNIT_PERCENT => percent(n, v as f32),
        UNIT_AUTO if auto.is_some() => (auto.unwrap())(n),
        // `auto` where yoga has no `*Auto` setter, and any value JS should never
        // send: reset, matching `dimNoAuto()`.
        _ => point(n, f32::NAN),
    }
}

/// Edge-parameterised twin of [`set_dim`] (margin / padding / inset).
unsafe fn set_dim_edge(
    n: YGNodeRef,
    edge: i32,
    v: f64,
    unit: f64,
    point: SetEdgeF,
    percent: SetEdgeF,
    auto: Option<SetEdgeAutoF>,
) {
    match unit as u32 {
        UNIT_UNDEFINED => point(n, edge, f32::NAN),
        UNIT_POINT => point(n, edge, v as f32),
        UNIT_PERCENT => percent(n, edge, v as f32),
        UNIT_AUTO if auto.is_some() => (auto.unwrap())(n, edge),
        _ => point(n, edge, f32::NAN),
    }
}

/// Issues the **same 34 yoga calls, in the same order**, as
/// `YogaLayoutEngine.applyStyle` does on a first apply
/// (`packages/ui/src/layout/YogaLayoutEngine.ts:222-301`) — including the engine
/// defaults it bakes in (column, shrink 0, align-items stretch), which are baked
/// into the *scratch* here because the defaulting happens on the JS side.
///
/// The order is load-bearing: `scripts/yoga-parity.ts` and the golden test compare
/// this against the wasm adapter, and yoga's setters are not all commutative with
/// respect to dirty propagation. Split out of the FFI entry point so the sequence
/// is reachable from tests without a JS context.
///
/// # Safety
/// `n` must be a live yoga node and `s` must hold at least [`STYLE_SLOTS`] values.
pub unsafe fn apply_style_to(n: YGNodeRef, s: &[f64]) {
    debug_assert!(s.len() >= STYLE_SLOTS);

    // 1-12: display, flex box, gaps.
    YGNodeStyleSetDisplay(n, display_of(s[0]));
    YGNodeStyleSetFlexDirection(n, flex_direction_of(s[1]));
    YGNodeStyleSetFlexWrap(n, wrap_of(s[2]));
    YGNodeStyleSetFlexGrow(n, s[3] as f32);
    YGNodeStyleSetFlexShrink(n, s[4] as f32);
    set_dim(
        n,
        s[5],
        s[6],
        YGNodeStyleSetFlexBasis,
        YGNodeStyleSetFlexBasisPercent,
        Some(YGNodeStyleSetFlexBasisAuto as SetAutoF),
    );
    YGNodeStyleSetJustifyContent(n, justify_of(s[7]));
    YGNodeStyleSetAlignItems(n, align_of(s[8]));
    YGNodeStyleSetAlignSelf(n, align_of(s[9]));
    YGNodeStyleSetAlignContent(n, align_content_of(s[10]));
    YGNodeStyleSetGap(n, YG_GUTTER_ROW, s[11] as f32);
    YGNodeStyleSetGap(n, YG_GUTTER_COLUMN, s[12] as f32);

    // 13-18: sizes. Only width/height accept `auto`.
    set_dim(
        n,
        s[13],
        s[14],
        YGNodeStyleSetWidth,
        YGNodeStyleSetWidthPercent,
        Some(YGNodeStyleSetWidthAuto as SetAutoF),
    );
    set_dim(
        n,
        s[15],
        s[16],
        YGNodeStyleSetHeight,
        YGNodeStyleSetHeightPercent,
        Some(YGNodeStyleSetHeightAuto as SetAutoF),
    );
    set_dim(n, s[17], s[18], YGNodeStyleSetMinWidth, YGNodeStyleSetMinWidthPercent, None);
    set_dim(n, s[19], s[20], YGNodeStyleSetMinHeight, YGNodeStyleSetMinHeightPercent, None);
    set_dim(n, s[21], s[22], YGNodeStyleSetMaxWidth, YGNodeStyleSetMaxWidthPercent, None);
    set_dim(n, s[23], s[24], YGNodeStyleSetMaxHeight, YGNodeStyleSetMaxHeightPercent, None);

    // 19-26: margin (auto is legal — it is how CSS centres a block) then padding.
    let margin = Some(YGNodeStyleSetMarginAuto as SetEdgeAutoF);
    set_dim_edge(n, YG_EDGE_TOP, s[25], s[26], YGNodeStyleSetMargin, YGNodeStyleSetMarginPercent, margin);
    set_dim_edge(n, YG_EDGE_RIGHT, s[27], s[28], YGNodeStyleSetMargin, YGNodeStyleSetMarginPercent, margin);
    set_dim_edge(n, YG_EDGE_BOTTOM, s[29], s[30], YGNodeStyleSetMargin, YGNodeStyleSetMarginPercent, margin);
    set_dim_edge(n, YG_EDGE_LEFT, s[31], s[32], YGNodeStyleSetMargin, YGNodeStyleSetMarginPercent, margin);
    set_dim_edge(n, YG_EDGE_TOP, s[33], s[34], YGNodeStyleSetPadding, YGNodeStyleSetPaddingPercent, None);
    set_dim_edge(n, YG_EDGE_RIGHT, s[35], s[36], YGNodeStyleSetPadding, YGNodeStyleSetPaddingPercent, None);
    set_dim_edge(n, YG_EDGE_BOTTOM, s[37], s[38], YGNodeStyleSetPadding, YGNodeStyleSetPaddingPercent, None);
    set_dim_edge(n, YG_EDGE_LEFT, s[39], s[40], YGNodeStyleSetPadding, YGNodeStyleSetPaddingPercent, None);

    // 27: CSS box model — the border ring takes LAYOUT space, so children can never
    // overlap the painted border (ui-rect draws it inward).
    YGNodeStyleSetBorder(n, YG_EDGE_ALL, s[41] as f32);

    // 28-32: position type + insets. `auto` insets are stripped JS-side (dimNoAuto),
    // so `YGNodeStyleSetPositionAuto` stays unused here exactly as in the adapter.
    YGNodeStyleSetPositionType(n, position_type_of(s[42]));
    set_dim_edge(n, YG_EDGE_TOP, s[43], s[44], YGNodeStyleSetPosition, YGNodeStyleSetPositionPercent, None);
    set_dim_edge(n, YG_EDGE_RIGHT, s[45], s[46], YGNodeStyleSetPosition, YGNodeStyleSetPositionPercent, None);
    set_dim_edge(n, YG_EDGE_BOTTOM, s[47], s[48], YGNodeStyleSetPosition, YGNodeStyleSetPositionPercent, None);
    set_dim_edge(n, YG_EDGE_LEFT, s[49], s[50], YGNodeStyleSetPosition, YGNodeStyleSetPositionPercent, None);

    // 33-34. NaN is `YGUndefined`, i.e. "no aspect ratio" — the wire says so
    // explicitly because `undefined` cannot survive an f64 lane any other way.
    YGNodeStyleSetAspectRatio(n, s[51] as f32);
    YGNodeStyleSetOverflow(n, overflow_of(s[52]));
}

// ---- measure trampoline ------------------------------------------------------

/// A measured extent JS handed back. NaN/negative would poison yoga's arithmetic
/// for the whole pass, so a nonsense measurement degrades to 0 rather than to an
/// unlaid-out tree.
fn sane_extent(v: f64) -> f32 {
    let v = v as f32;
    if v.is_finite() && v > 0.0 { v } else { 0.0 }
}

/// Yoga's `YGMeasureFunc`, called from inside `YGNodeCalculateLayout`.
///
/// `extern "C"` (MS x64), NOT `sysv64` — the caller is clang-cl-compiled C++, not
/// JSC. Everything it needs hangs off the node's context pointer, so it never
/// derives a second `&mut WorldData` (see the module header).
unsafe extern "C" fn measure_trampoline(
    node: YGNodeConstRef,
    width: f32,
    width_mode: i32,
    height: f32,
    height_mode: i32,
) -> YGSize {
    const ZERO: YGSize = YGSize { width: 0.0, height: 0.0 };

    let ctx = YGNodeGetContext(node) as *mut NodeCtx;
    if ctx.is_null() {
        return ZERO;
    }
    let func = (*ctx).func;
    let bus = (*ctx).bus;
    if !is_cell(func) || bus.is_null() {
        return ZERO;
    }
    let g = (*bus).g;

    // `width`/`height` are YGUndefined (NaN) when the axis is unconstrained; the
    // encoder round-trips NaN unchanged, so JS sees the same value the wasm
    // adapter passes it. The modes are yoga's ordinals, which are identical to
    // `UIMeasureMode` (Undefined=0/Exactly=1/AtMost=2).
    let args = [
        enc_f64(width as f64),
        enc_f64(width_mode as f64),
        enc_f64(height as f64),
        enc_f64(height_mode as f64),
    ];
    let mut out = UNDEF;
    if crate::glue::rjs_call(g, func, UNDEF, args.as_ptr(), args.len() as u32, &mut out) != 0 {
        // Park the FIRST exception (later leaves are measured as 0×0 so layout
        // still terminates) and let `calculate_layout` re-throw it. Throwing from
        // here would unwind through yoga's C++ frames.
        if !is_cell((*bus).pending_exc) && is_cell(out) {
            crate::ffi::redjs_protect(g, out);
            (*bus).pending_exc = out;
        }
        return ZERO;
    }

    // Result comes back through the scratch, not as an object: `[0]` = width,
    // `[1]` = height. No allocation, no property lookup, one crossing.
    let scratch = (*bus).scratch;
    YGSize { width: sane_extent(*scratch), height: sane_extent(*scratch.add(1)) }
}

// ---- lifecycle ---------------------------------------------------------------

unsafe extern "sysv64" fn ctor(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some((cls, default_proto)) = class_of(g) else {
        return throw_type_error(g, "RedYoga not installed");
    };
    let mut proto = get(g, cf_new_target(cf), "prototype");
    if !is_cell(proto) {
        proto = default_proto;
    }
    let wd = WorldData::new(g);
    host_create::<WorldData>(g, cls, proto, wd)
}

/// Explicit teardown — the supported path, because it is the only one that may
/// touch JS (detach the scratch views, unprotect the measure functions).
unsafe extern "sysv64" fn free(g: *mut c_void, cf: *mut c_void) -> u64 {
    if in_layout() {
        return throw_type_error(g, "RedYoga: cannot free a world during layout");
    }
    let Some((cls, _)) = class_of(g) else { return UNDEF };
    let Some(wd) = host_data::<WorldData>(g, cls, cf_this(cf)) else {
        return throw_type_error(g, "RedYoga method called on wrong this");
    };
    if wd.dead {
        return UNDEF;
    }
    wd.dead = true;

    let recs: Vec<NodeRec> = wd.nodes.iter().map(|(_, r)| *r).collect();
    wd.nodes.clear();
    for rec in recs {
        wd.drop_node(rec, true);
    }
    if is_cell((*wd.bus).pending_exc) {
        crate::ffi::redjs_unprotect(g, (*wd.bus).pending_exc);
        (*wd.bus).pending_exc = UNDEF;
    }
    wd.scratch.release(g);
    drop(Box::from_raw(wd.bus));
    wd.bus = core::ptr::null_mut();
    UNDEF
}

// ---- views -------------------------------------------------------------------

unsafe extern "sysv64" fn get_generation(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    enc_f64(wd.generation as f64)
}

unsafe extern "sysv64" fn get_scratch(g: *mut c_void, cf: *mut c_void) -> u64 {
    world!(g, cf).scratch.view()
}

// ---- tree --------------------------------------------------------------------

unsafe extern "sysv64" fn create_node(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let node = YGNodeNew();
    if node.is_null() {
        return throw_type_error(g, "RedYoga: node allocation failed");
    }
    // No context and no measure func yet — text leaves pay for the `NodeCtx` box,
    // everything else does not.
    enc_f64(wd.nodes.insert(NodeRec { node, ctx: core::ptr::null_mut() }) as f64)
}

/// Frees ONE node. Mirrors `ILayoutNode.free`'s contract — `UIDocument.freeSubtree`
/// detaches from the parent first (`packages/ui/src/tree/UIDocument.ts:260-273`) —
/// but does not rely on it: `YGNodeFree` removes the node from its owner itself,
/// and orphans any children, whose slots stay valid.
unsafe extern "sysv64" fn free_node(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let slot = arg_u(cf, 0);
    let Some(rec) = wd.nodes.remove(slot) else {
        return throw_type_error(g, "RedYoga: invalid node slot");
    };
    wd.drop_node(rec, true);
    UNDEF
}

unsafe extern "sysv64" fn insert_child(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let parent = node_of!(g, wd, arg_u(cf, 0));
    let child = node_of!(g, wd, arg_u(cf, 1));
    if parent == child {
        return throw_type_error(g, "RedYoga: cannot insert a node into itself");
    }
    // The two states yoga aborts the process on (`cpp/yoga/YGNode.cpp:135,140`).
    if !YGNodeGetOwner(child).is_null() {
        return throw_type_error(g, "RedYoga: child already has a parent — remove it first");
    }
    if YGNodeHasMeasureFunc(parent) {
        return throw_type_error(g, "RedYoga: nodes with a measure function cannot have children");
    }
    // yoga inserts at `begin() + index` without a bounds check; clamping is the
    // difference between an append and a heap overwrite.
    let count = YGNodeGetChildCount(parent);
    let index = (arg_f(cf, 2).max(0.0) as usize).min(count);
    YGNodeInsertChild(parent, child, index);
    UNDEF
}

unsafe extern "sysv64" fn remove_child(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let parent = node_of!(g, wd, arg_u(cf, 0));
    let child = node_of!(g, wd, arg_u(cf, 1));
    // A no-op when `child` is not a child of `parent` — yoga checks itself.
    YGNodeRemoveChild(parent, child);
    UNDEF
}

// ---- style -------------------------------------------------------------------

unsafe extern "sysv64" fn apply_style(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let node = node_of!(g, wd, arg_u(cf, 0));
    apply_style_to(node, &wd.scratch.data()[..STYLE_SLOTS]);
    UNDEF
}

unsafe extern "sysv64" fn set_measure_func(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let slot = arg_u(cf, 0);
    let Some(rec) = wd.nodes.get(slot).copied() else {
        return throw_type_error(g, "RedYoga: invalid node slot");
    };
    let fn_v = cf_arg(cf, 1);
    let clearing = !is_cell(fn_v);
    if !clearing && crate::glue::rjs_kind(g, fn_v) != KIND_FUNCTION {
        return throw_type_error(g, "RedYoga: measure function must be callable or null");
    }
    // Yoga aborts the process when a node with children gets a measure func
    // (`cpp/yoga/node/Node.cpp:106-110`).
    if !clearing && YGNodeGetChildCount(rec.node) > 0 {
        return throw_type_error(g, "RedYoga: nodes with children cannot have a measure function");
    }

    if clearing {
        if !rec.ctx.is_null() {
            YGNodeSetMeasureFunc(rec.node, None);
            YGNodeSetContext(rec.node, core::ptr::null_mut());
            let ctx = Box::from_raw(rec.ctx);
            if is_cell(ctx.func) {
                crate::ffi::redjs_unprotect(g, ctx.func);
            }
            if let Some(r) = wd.nodes.get_mut(slot) {
                r.ctx = core::ptr::null_mut();
            }
        }
        return UNDEF;
    }

    crate::ffi::redjs_protect(g, fn_v);
    if rec.ctx.is_null() {
        let ctx = Box::into_raw(Box::new(NodeCtx { bus: wd.bus, func: fn_v }));
        if let Some(r) = wd.nodes.get_mut(slot) {
            r.ctx = ctx;
        }
        YGNodeSetContext(rec.node, ctx as *mut c_void);
    } else {
        let old = (*rec.ctx).func;
        (*rec.ctx).func = fn_v;
        if is_cell(old) {
            crate::ffi::redjs_unprotect(g, old);
        }
    }
    // Set last: until the context is in place the trampoline would measure 0×0.
    YGNodeSetMeasureFunc(rec.node, Some(measure_trampoline));
    UNDEF
}

unsafe extern "sysv64" fn mark_dirty(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let node = node_of!(g, wd, arg_u(cf, 0));
    // Yoga aborts on `markDirty` without a measure func (`cpp/yoga/YGNode.cpp:111`).
    // Non-leaf dirtying happens implicitly through the style setters.
    if !YGNodeHasMeasureFunc(node) {
        return throw_type_error(g, "RedYoga: markDirty needs a node with a measure function");
    }
    YGNodeMarkDirty(node);
    UNDEF
}

// ---- layout ------------------------------------------------------------------

unsafe extern "sysv64" fn calculate_layout(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let node = node_of!(g, wd, arg_u(cf, 0));
    let w = arg_avail(cf, 1);
    let h = arg_avail(cf, 2);
    let bus = wd.bus;

    // Direction is pinned LTR, matching the adapter (`YogaLayoutEngine.ts:318`);
    // RTL is a UI-layer concern that has not reached this seam.
    IN_LAYOUT.with(|c| c.set(c.get() + 1));
    YGNodeCalculateLayout(node, w, h, YG_DIRECTION_LTR);
    IN_LAYOUT.with(|c| c.set(c.get() - 1));

    // A measure callback that threw: surface it now that no C++ frame is on the
    // stack. Layout DID complete (those leaves measured 0×0), so the tree is in a
    // defined state and the next pass can recover.
    let exc = (*bus).pending_exc;
    if is_cell(exc) {
        (*bus).pending_exc = UNDEF;
        crate::glue::rjs_throw(g, exc);
        // The VM's exception slot roots it from here on.
        crate::ffi::redjs_unprotect(g, exc);
    }
    UNDEF
}

/// The batched replacement for `hasNewLayout` + 4 getters + `markLayoutSeen`
/// (design doc §3, "Is the seam sufficient?" — a native backend must implement the
/// flag faithfully, and must not clear it for nodes the walk never visited).
/// Returns 1 and fills `scratch[0..4]` with `[left, top, width, height]` when the
/// node reported a new layout, then clears the flag; 0 and no writes otherwise.
///
/// One yoga behaviour the JS side should not be surprised by: the node
/// `calculateLayout` ran on re-raises its own flag on EVERY pass, clean or not
/// (`cpp/yoga/algorithm/CalculateLayout.cpp:2311-2320`). The wasm adapter has the
/// same behaviour — it is the same yoga — so this is parity, not a regression.
unsafe extern "sysv64" fn sync_node(g: *mut c_void, cf: *mut c_void) -> u64 {
    let wd = world!(g, cf);
    let node = node_of!(g, wd, arg_u(cf, 0));
    if !YGNodeGetHasNewLayout(node) {
        return enc_f64(0.0);
    }
    let s = wd.scratch.data();
    s[0] = YGNodeLayoutGetLeft(node) as f64;
    s[1] = YGNodeLayoutGetTop(node) as f64;
    s[2] = YGNodeLayoutGetWidth(node) as f64;
    s[3] = YGNodeLayoutGetHeight(node) as f64;
    YGNodeSetHasNewLayout(node, false);
    enc_f64(1.0)
}

// ---- install -----------------------------------------------------------------

pub unsafe fn install(g: *mut c_void) {
    if states().lock().unwrap().contains_key(&(g as usize)) {
        return;
    }
    let cls = make_host_class::<WorldData>("RedYoga");
    let class = define_class(
        g,
        &ClassSpec {
            name: "RedYoga",
            arity: 0,
            construct: Some(ctor),
            parent_proto: 0,
            methods: &[
                ("free", 0, free),
                ("getGeneration", 0, get_generation),
                ("getScratch", 0, get_scratch),
                ("createNode", 0, create_node),
                ("freeNode", 1, free_node),
                ("insertChild", 3, insert_child),
                ("removeChild", 2, remove_child),
                ("applyStyle", 1, apply_style),
                ("setMeasureFunc", 2, set_measure_func),
                ("markDirty", 1, mark_dirty),
                ("calculateLayout", 3, calculate_layout),
                ("syncNode", 1, sync_node),
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

// ---- tests -------------------------------------------------------------------
//
// These drive yoga directly (no JS context needed), which is exactly what makes
// them worth having: they pin the wire→yoga ordinal mapping, the one part of this
// file that is silent when wrong — a mis-mapped enum lays out plausibly rather
// than failing.

#[cfg(test)]
mod tests {
    use super::*;

    /// The scratch the adapter writes for a node with no layout declarations at
    /// all: yoga/RN defaults, i.e. `column`, `shrink: 0`, `align-items: stretch`,
    /// `align-content: flex-start`, everything else undefined
    /// (`YogaLayoutEngine.ts:222-301`, the `??` fallbacks).
    fn default_style() -> Vec<f64> {
        let mut s = vec![0.0f64; STYLE_SLOTS];
        s[8] = 4.0; // alignItems: stretch
        s[10] = 1.0; // alignContent: flex-start
        s[51] = f64::NAN; // aspectRatio: undefined
        s
    }

    fn point(s: &mut [f64], value_slot: usize, v: f64) {
        s[value_slot] = v;
        s[value_slot + 1] = UNIT_POINT as f64;
    }

    #[test]
    fn default_style_stacks_children_in_a_column_and_stretches_them() {
        unsafe {
            let root = YGNodeNew();
            let a = YGNodeNew();
            let b = YGNodeNew();
            apply_style_to(root, &default_style());
            let mut child = default_style();
            point(&mut child, 15, 20.0); // height: 20
            apply_style_to(a, &child);
            apply_style_to(b, &child);
            YGNodeInsertChild(root, a, 0);
            YGNodeInsertChild(root, b, 1);

            YGNodeCalculateLayout(root, 200.0, 100.0, YG_DIRECTION_LTR);

            assert_eq!(YGNodeLayoutGetTop(a), 0.0);
            assert_eq!(YGNodeLayoutGetTop(b), 20.0, "column direction (wire 0) must be YGFlexDirectionColumn");
            assert_eq!(YGNodeLayoutGetWidth(a), 200.0, "align-items stretch (wire 4) must be YGAlignStretch");
            YGNodeFree(a);
            YGNodeFree(b);
            YGNodeFree(root);
        }
    }

    #[test]
    fn row_direction_and_percent_and_auto_units_reach_the_right_setters() {
        unsafe {
            let root = YGNodeNew();
            let a = YGNodeNew();
            let b = YGNodeNew();
            let mut rs = default_style();
            rs[1] = 2.0; // flexDirection: row
            apply_style_to(root, &rs);

            let mut pct = default_style();
            pct[13] = 50.0;
            pct[14] = UNIT_PERCENT as f64;
            apply_style_to(a, &pct);

            // width: auto with flexGrow 1 — `auto` must take YGNodeStyleSetWidthAuto,
            // not the point setter with 3.0.
            let mut auto = default_style();
            auto[13] = 0.0;
            auto[14] = UNIT_AUTO as f64;
            auto[3] = 1.0; // flexGrow
            apply_style_to(b, &auto);

            YGNodeInsertChild(root, a, 0);
            YGNodeInsertChild(root, b, 1);
            YGNodeCalculateLayout(root, 200.0, 100.0, YG_DIRECTION_LTR);

            assert_eq!(YGNodeLayoutGetWidth(a), 100.0);
            assert_eq!(YGNodeLayoutGetLeft(b), 100.0);
            assert_eq!(YGNodeLayoutGetWidth(b), 100.0);
            YGNodeFree(a);
            YGNodeFree(b);
            YGNodeFree(root);
        }
    }

    #[test]
    fn absolute_position_maps_onto_yogas_offset_position_enum() {
        // The trap this test exists for: YGPositionType is Static=0/Relative=1/
        // Absolute=2, so passing the wire's 1 straight through would make an
        // absolutely-positioned node RELATIVE and its insets no-ops.
        unsafe {
            let root = YGNodeNew();
            let a = YGNodeNew();
            apply_style_to(root, &default_style());
            let mut s = default_style();
            s[42] = 1.0; // position: absolute
            point(&mut s, 43, 5.0); // top: 5
            point(&mut s, 49, 10.0); // left: 10
            point(&mut s, 13, 30.0); // width: 30
            point(&mut s, 15, 30.0); // height: 30
            apply_style_to(a, &s);
            YGNodeInsertChild(root, a, 0);

            YGNodeCalculateLayout(root, 200.0, 100.0, YG_DIRECTION_LTR);

            assert_eq!(YGNodeLayoutGetLeft(a), 10.0);
            assert_eq!(YGNodeLayoutGetTop(a), 5.0);
            YGNodeFree(a);
            YGNodeFree(root);
        }
    }

    #[test]
    fn undefined_unit_resets_a_dimension_set_by_an_earlier_apply() {
        // `applyStyle` is a full re-apply, so "the field went away" is expressed as
        // unit 0 — which must reach the point setter with YGUndefined, not be skipped.
        unsafe {
            let root = YGNodeNew();
            let a = YGNodeNew();
            apply_style_to(root, &default_style());
            let mut s = default_style();
            point(&mut s, 13, 50.0);
            apply_style_to(a, &s);
            YGNodeInsertChild(root, a, 0);
            YGNodeCalculateLayout(root, 200.0, 100.0, YG_DIRECTION_LTR);
            assert_eq!(YGNodeLayoutGetWidth(a), 50.0);

            apply_style_to(a, &default_style());
            YGNodeCalculateLayout(root, 200.0, 100.0, YG_DIRECTION_LTR);
            assert_eq!(YGNodeLayoutGetWidth(a), 200.0, "unit 0 must reset width back to stretch");
            YGNodeFree(a);
            YGNodeFree(root);
        }
    }

    #[test]
    fn has_new_layout_is_sticky_until_explicitly_cleared() {
        // What `syncNode` rides on: yoga raises the flag and ONLY an explicit clear
        // takes it down, so a batched read-back cannot silently clear it for nodes
        // the walk never visited (design doc §3, "Is the seam sufficient?").
        unsafe {
            let root = YGNodeNew();
            let a = YGNodeNew();
            apply_style_to(root, &default_style());
            apply_style_to(a, &default_style());
            YGNodeInsertChild(root, a, 0);
            YGNodeCalculateLayout(root, 200.0, 100.0, YG_DIRECTION_LTR);
            assert!(YGNodeGetHasNewLayout(root));
            assert!(YGNodeGetHasNewLayout(a));

            // Clear only the child — the parent's flag must survive untouched,
            // which is what lets `refreshRects` walk in any order.
            YGNodeSetHasNewLayout(a, false);
            assert!(!YGNodeGetHasNewLayout(a));
            assert!(YGNodeGetHasNewLayout(root));

            // A clean re-solve re-flags the ROOT unconditionally (yoga does this in
            // `calculateLayoutInternal`'s `performLayout` tail, CalculateLayout.cpp:2311-2320)
            // but leaves the untouched child alone. Same behaviour as the wasm
            // adapter — same yoga — so it is parity, not a defect.
            YGNodeSetHasNewLayout(root, false);
            YGNodeCalculateLayout(root, 200.0, 100.0, YG_DIRECTION_LTR);
            assert!(YGNodeGetHasNewLayout(root));
            assert!(!YGNodeGetHasNewLayout(a));
            YGNodeFree(a);
            YGNodeFree(root);
        }
    }
}
