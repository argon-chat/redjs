//! The JS side of `DelayNode`, `WaveShaperNode`, `DynamicsCompressorNode`, `ConvolverNode` and
//! `PeriodicWave`: factories, constructors and attributes. The DSP is in `effects.rs`.

use super::effects::{CompShared, ConvShared, DelayShared, ShaperShared, OVERSAMPLE};
use super::*;

pub(super) struct Classes {
    pub delay: Class,
    pub shaper: Class,
    pub compressor: Class,
    pub convolver: Class,
    pub wave: Class,
    pub wave_cls: crate::capi::JSClassRef,
}

struct WaveData {
    table: ArcGc<WaveTable>,
}

macro_rules! slot_getter {
    ($fn_name:ident, $slot:literal) => {
        #[crate::host_fn]
        unsafe fn $fn_name(g: *mut c_void, cf: *mut c_void) -> u64 {
            get(g, slots_of(g, cf_this(cf)), $slot)
        }
    };
}

const COMPRESSOR_PARAMS: [(&str, u8, f64, f64, f64); 5] = [
    ("attack", P_ATTACK, 0.003, 0.0, 1.0),
    ("knee", P_KNEE, 30.0, 0.0, 40.0),
    ("ratio", P_RATIO, 12.0, 1.0, 20.0),
    ("release", P_RELEASE, 0.25, 0.0, 1.0),
    ("threshold", P_THRESHOLD, -24.0, -100.0, 0.0),
];

unsafe fn proto_for(g: *mut c_void, cf: *mut c_void, fallback: u64) -> u64 {
    let nt = cf_new_target(cf);
    let proto = if is_cell(nt) {
        get(g, nt, "prototype")
    } else {
        UNDEF
    };
    if is_cell(proto) {
        proto
    } else {
        fallback
    }
}

/// A constructor's first argument. `Err` has thrown.
unsafe fn context_arg(g: *mut c_void, cf: *mut c_void, iface: &str) -> Result<u64, ()> {
    engine_of(g, cf_arg(cf, 0)).ok_or_else(|| {
        throw_type_error(
            g,
            &format!(
                "Failed to construct '{iface}': parameter 1 is not of type 'BaseAudioContext'."
            ),
        );
    })
}

/// A constructor's options dictionary: `None` for `undefined` and `null`.
unsafe fn options_arg(g: *mut c_void, cf: *mut c_void, iface: &str) -> Result<Option<u64>, ()> {
    let v = cf_arg(cf, 1);
    if v == UNDEF || v == NULL {
        return Ok(None);
    }
    if !super::super::blob::is_object(g, v) {
        throw_type_error(
            g,
            &format!("Failed to construct '{iface}': The provided value is not a dictionary."),
        );
        return Err(());
    }
    Ok(Some(v))
}

unsafe fn member(g: *mut c_void, dict: Option<u64>, name: &str) -> Result<u64, ()> {
    match dict {
        Some(d) => try_get(g, d, name),
        None => Ok(UNDEF),
    }
}

/// A `double` or `float` member: absent is `None`, a non-finite value a TypeError.
unsafe fn number_member(g: *mut c_void, dict: Option<u64>, name: &str) -> Result<Option<f64>, ()> {
    let v = member(g, dict, name)?;
    if v == UNDEF {
        return Ok(None);
    }
    let n = to_number(g, v)?;
    if !n.is_finite() {
        throw_type_error(
            g,
            &format!("The provided value for '{name}' is non-finite."),
        );
        return Err(());
    }
    Ok(Some(n))
}

/// A `Float32Array` or `sequence<float>`: `None` for `undefined` and `null`.
unsafe fn floats(g: *mut c_void, v: u64) -> Result<Option<Vec<f32>>, ()> {
    if v == UNDEF || v == NULL {
        return Ok(None);
    }
    let mut ta = crate::ffi::RjsTaInfo::default();
    if glue::rjs_typedarray_info(g, v, &mut ta) == 0 && ta.kind == 8 {
        if ta.data.is_null() {
            return Ok(Some(Vec::new()));
        }
        let data = core::slice::from_raw_parts(ta.data as *const f32, ta.length as usize);
        return Ok(Some(data.to_vec()));
    }
    if !super::super::blob::is_object(g, v) {
        throw_type_error(g, "The provided value is not a sequence of numbers.");
        return Err(());
    }
    let len = to_number(g, try_get(g, v, "length")?)?;
    let len = if len.is_finite() {
        len.clamp(0.0, 1e7) as u32
    } else {
        0
    };
    let mut out = Vec::with_capacity(len as usize);
    for i in 0..len {
        out.push(to_number(g, get_i(g, v, i))? as f32);
    }
    Ok(Some(out))
}

unsafe fn float32_array(g: *mut c_void, data: &[f32]) -> u64 {
    let mut out = UNDEF;
    if crate::ffi::redjs_typedarray_create(g, 8, data.len(), &mut out) != 0 {
        return UNDEF;
    }
    let mut ta = crate::ffi::RjsTaInfo::default();
    if glue::rjs_typedarray_info(g, out, &mut ta) == 0 && !ta.data.is_null() {
        core::ptr::copy_nonoverlapping(data.as_ptr(), ta.data as *mut f32, data.len());
    }
    out
}

/// The `AudioNodeOptions` of a constructor, applied to the node just made.
unsafe fn apply_node_options(
    g: *mut c_void,
    node: u64,
    dict: Option<u64>,
    iface: &str,
) -> Result<(), ()> {
    let Some(d) = dict else { return Ok(()) };
    let count = try_get(g, d, "channelCount")?;
    let count = if count == UNDEF {
        None
    } else {
        Some(idl_u32(g, count, 0)?)
    };
    let mode = enum_member(
        g,
        d,
        "channelCountMode",
        COUNT_MODES,
        "ChannelCountMode",
        iface,
    )?;
    let interp = enum_member(
        g,
        d,
        "channelInterpretation",
        INTERPRETATIONS,
        "ChannelInterpretation",
        iface,
    )?;
    let Some(h) = host_data::<NodeHandle>(g, classes(g).node_cls, node).map(|h| h.0) else {
        return Ok(());
    };
    let refused = {
        let mut gl = glob().lock().unwrap();
        let gl = &mut *gl;
        let Some(rec) = gl.nodes.get_mut(&h) else {
            return Ok(());
        };
        let limit = host_channel_limit(rec);
        let fixed = matches!(rec.params, Params::Compressor(_) | Params::Convolver(_));
        if let Some(n) = count.filter(|&n| n == 0 || n > limit) {
            Some(format!(
                "The channel count provided ({n}) is outside the range [1, {limit}]."
            ))
        } else if fixed && mode.as_deref() == Some("max") {
            Some("channelCountMode cannot be 'max'.".into())
        } else {
            if let Some(n) = count {
                rec.ch_count = n;
            }
            if let Some(m) = mode.as_deref().and_then(CountMode::parse) {
                rec.ch_mode = m;
            }
            if let Some(i) = interp.as_deref().and_then(Interp::parse) {
                rec.ch_interp = i;
            }
            let eh = rec.engine;
            graph::resync(&mut gl.engines, &mut gl.nodes, eh);
            None
        }
    };
    match refused {
        Some(m) => {
            super::super::domexception::throw(
                g,
                "NotSupportedError",
                &format!("Failed to construct '{iface}': {m}"),
            );
            Err(())
        }
        None => Ok(()),
    }
}

fn sample_rate_of(engine: u64) -> u32 {
    glob()
        .lock()
        .unwrap()
        .engines
        .get(&engine)
        .map_or(48_000, |e| e.sample_rate)
}

// ---------------------------------------------------------------------------
// DelayNode
// ---------------------------------------------------------------------------

unsafe fn make_delay(
    g: *mut c_void,
    engine: u64,
    proto: u64,
    max: f64,
    delay: f64,
    what: &str,
) -> u64 {
    if !(max > 0.0 && max < 180.0) {
        return super::super::domexception::throw(
            g,
            "NotSupportedError",
            &format!("{what}: the maximum delay time ({max}) is outside the range (0, 180)."),
        );
    }
    let delay = delay.clamp(0.0, max);
    let shared = ArcGc::new(DelayShared::new(
        2,
        max,
        delay as f32,
        sample_rate_of(engine),
    ));
    let params = Params::Delay {
        shared,
        writer: NodeID::DANGLING,
    };
    create_node_on(g, engine, params, proto, &[("delayTime", P_DELAY, delay)])
}

#[crate::host_fn]
pub(super) unsafe fn create_delay(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(engine) = this_engine_handle(g, cf) else {
        return throw_type_error(g, "AudioContext.createDelay on wrong this");
    };
    let max = if cf_arg(cf, 0) == UNDEF {
        1.0
    } else {
        let Ok(v) = to_number(g, cf_arg(cf, 0)) else {
            return UNDEF;
        };
        v
    };
    let proto = classes(g).fx.delay.proto;
    make_delay(
        g,
        engine,
        proto,
        max,
        0.0,
        "Failed to execute 'createDelay' on 'BaseAudioContext'",
    )
}

#[crate::host_fn]
unsafe fn delay_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let iface = "DelayNode";
    let Ok(engine) = context_arg(g, cf, iface) else {
        return UNDEF;
    };
    let Ok(dict) = options_arg(g, cf, iface) else {
        return UNDEF;
    };
    let Ok(delay) = number_member(g, dict, "delayTime") else {
        return UNDEF;
    };
    let Ok(max) = number_member(g, dict, "maxDelayTime") else {
        return UNDEF;
    };
    let proto = proto_for(g, cf, classes(g).fx.delay.proto);
    let node = make_delay(
        g,
        engine,
        proto,
        max.unwrap_or(1.0),
        delay.unwrap_or(0.0),
        "Failed to construct 'DelayNode'",
    );
    if !is_cell(node) || apply_node_options(g, node, dict, iface).is_err() {
        return UNDEF;
    }
    node
}

slot_getter!(get_slot_delay_time, "delayTime");

// ---------------------------------------------------------------------------
// WaveShaperNode
// ---------------------------------------------------------------------------

unsafe fn shaper_of(g: *mut c_void, cf: *mut c_void) -> Option<ArcGc<ShaperShared>> {
    let h = this_node_handle(g, cf)?;
    match &glob().lock().unwrap().nodes.get(&h)?.params {
        Params::Shaper(sh) => Some(ArcGc::clone(sh)),
        _ => None,
    }
}

unsafe fn make_shaper(g: *mut c_void, engine: u64, proto: u64) -> u64 {
    let params = Params::Shaper(ArcGc::new(ShaperShared::default()));
    create_node_on(g, engine, params, proto, &[])
}

#[crate::host_fn]
pub(super) unsafe fn create_wave_shaper(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(engine) = this_engine_handle(g, cf) else {
        return throw_type_error(g, "AudioContext.createWaveShaper on wrong this");
    };
    make_shaper(g, engine, classes(g).fx.shaper.proto)
}

/// A curve of fewer than two points is an InvalidStateError; `None` clears it.
unsafe fn set_curve(g: *mut c_void, sh: &ShaperShared, curve: Option<Vec<f32>>) -> Result<(), ()> {
    if curve.as_ref().is_some_and(|c| c.len() < 2) {
        super::super::domexception::throw(
            g,
            "InvalidStateError",
            "WaveShaperNode.curve needs at least two values.",
        );
        return Err(());
    }
    sh.curve.set(curve.map(ArcGc::new));
    Ok(())
}

#[crate::host_fn]
unsafe fn shaper_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let iface = "WaveShaperNode";
    let Ok(engine) = context_arg(g, cf, iface) else {
        return UNDEF;
    };
    let Ok(dict) = options_arg(g, cf, iface) else {
        return UNDEF;
    };
    let Ok(curve) = member(g, dict, "curve") else {
        return UNDEF;
    };
    let Ok(curve) = floats(g, curve) else {
        return UNDEF;
    };
    let oversample = match dict {
        Some(d) => match enum_member(g, d, "oversample", OVERSAMPLE, "OverSampleType", iface) {
            Ok(v) => v,
            Err(()) => return UNDEF,
        },
        None => None,
    };
    let shared = ArcGc::new(ShaperShared::default());
    if set_curve(g, &shared, curve).is_err() {
        return UNDEF;
    }
    if let Some(i) = oversample.and_then(|o| OVERSAMPLE.iter().position(|&n| n == o)) {
        shared.oversample.store(i as u32, Ordering::Relaxed);
    }
    let proto = proto_for(g, cf, classes(g).fx.shaper.proto);
    let node = create_node_on(g, engine, Params::Shaper(shared), proto, &[]);
    if !is_cell(node) || apply_node_options(g, node, dict, iface).is_err() {
        return UNDEF;
    }
    node
}

#[crate::host_fn]
unsafe fn shaper_get_curve(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(sh) = shaper_of(g, cf) else {
        return throw_type_error(g, "WaveShaperNode.curve on wrong this");
    };
    match sh.curve.get() {
        Some(curve) => float32_array(g, &curve),
        None => NULL,
    }
}

#[crate::host_fn]
unsafe fn shaper_set_curve(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(sh) = shaper_of(g, cf) else {
        return throw_type_error(g, "WaveShaperNode.curve on wrong this");
    };
    let v = cf_arg(cf, 0);
    let mut ta = crate::ffi::RjsTaInfo::default();
    let typed = glue::rjs_typedarray_info(g, v, &mut ta) == 0 && ta.kind == 8;
    if v != NULL && !typed {
        return throw_type_error(
            g,
            "Failed to set the 'curve' property on 'WaveShaperNode': The provided value is not of \
             type 'Float32Array'.",
        );
    }
    if let Ok(curve) = floats(g, v) {
        let _ = set_curve(g, &sh, curve);
    }
    UNDEF
}

#[crate::host_fn]
unsafe fn shaper_get_oversample(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(sh) = shaper_of(g, cf) else {
        return throw_type_error(g, "WaveShaperNode.oversample on wrong this");
    };
    let i = sh.oversample.load(Ordering::Relaxed) as usize;
    js_string(g, OVERSAMPLE[i.min(OVERSAMPLE.len() - 1)])
}

#[crate::host_fn]
unsafe fn shaper_set_oversample(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(sh) = shaper_of(g, cf) else {
        return throw_type_error(g, "WaveShaperNode.oversample on wrong this");
    };
    if let Ok(Some(v)) = enum_value(g, cf_arg(cf, 0), OVERSAMPLE) {
        if let Some(i) = OVERSAMPLE.iter().position(|&n| n == v) {
            sh.oversample.store(i as u32, Ordering::Relaxed);
        }
    }
    UNDEF
}

// ---------------------------------------------------------------------------
// DynamicsCompressorNode
// ---------------------------------------------------------------------------

unsafe fn make_compressor(g: *mut c_void, engine: u64, proto: u64, values: [f64; 5]) -> u64 {
    let sh = CompShared::default();
    let mut jsparams = [("", 0u8, 0.0f64); 5];
    for (k, (&(name, kind, _, lo, hi), v)) in COMPRESSOR_PARAMS.iter().zip(values).enumerate() {
        let v = v.clamp(lo, hi);
        jsparams[k] = (name, kind, v);
        let cell = match kind {
            P_ATTACK => &sh.attack,
            P_KNEE => &sh.knee,
            P_RATIO => &sh.ratio,
            P_RELEASE => &sh.release,
            _ => &sh.threshold,
        };
        cell.set(v as f32);
    }
    create_node_on(
        g,
        engine,
        Params::Compressor(ArcGc::new(sh)),
        proto,
        &jsparams,
    )
}

#[crate::host_fn]
pub(super) unsafe fn create_dynamics_compressor(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(engine) = this_engine_handle(g, cf) else {
        return throw_type_error(g, "AudioContext.createDynamicsCompressor on wrong this");
    };
    let defaults = COMPRESSOR_PARAMS.map(|p| p.2);
    make_compressor(g, engine, classes(g).fx.compressor.proto, defaults)
}

#[crate::host_fn]
unsafe fn compressor_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let iface = "DynamicsCompressorNode";
    let Ok(engine) = context_arg(g, cf, iface) else {
        return UNDEF;
    };
    let Ok(dict) = options_arg(g, cf, iface) else {
        return UNDEF;
    };
    let mut values = COMPRESSOR_PARAMS.map(|p| p.2);
    for (v, (name, ..)) in values.iter_mut().zip(COMPRESSOR_PARAMS) {
        match number_member(g, dict, name) {
            Ok(Some(n)) => *v = n,
            Ok(None) => {}
            Err(()) => return UNDEF,
        }
    }
    let proto = proto_for(g, cf, classes(g).fx.compressor.proto);
    let node = make_compressor(g, engine, proto, values);
    if !is_cell(node) || apply_node_options(g, node, dict, iface).is_err() {
        return UNDEF;
    }
    node
}

slot_getter!(get_slot_threshold, "threshold");
slot_getter!(get_slot_knee, "knee");
slot_getter!(get_slot_ratio, "ratio");
slot_getter!(get_slot_attack, "attack");
slot_getter!(get_slot_release, "release");

#[crate::host_fn]
unsafe fn compressor_get_reduction(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(h) = this_node_handle(g, cf) else {
        return throw_type_error(g, "DynamicsCompressorNode.reduction on wrong this");
    };
    let gl = glob().lock().unwrap();
    match gl.nodes.get(&h).map(|r| &r.params) {
        Some(Params::Compressor(sh)) => enc_f64(sh.reduction.get() as f64),
        _ => enc_f64(0.0),
    }
}

// ---------------------------------------------------------------------------
// ConvolverNode
// ---------------------------------------------------------------------------

unsafe fn convolver_of(g: *mut c_void, v: u64) -> Option<(u64, ArcGc<ConvShared>)> {
    let h = host_data::<NodeHandle>(g, classes(g).node_cls, v)?.0;
    match &glob().lock().unwrap().nodes.get(&h)?.params {
        Params::Convolver(sh) => Some((h, ArcGc::clone(sh))),
        _ => None,
    }
}

/// `buffer = value` on the convolver `node`. `Err` has thrown.
unsafe fn set_response(g: *mut c_void, node: u64, value: u64) -> Result<(), ()> {
    let Some((h, sh)) = convolver_of(g, node) else {
        throw_type_error(g, "ConvolverNode.buffer on wrong this");
        return Err(());
    };
    let buf = host_data::<BufData>(g, classes(g).buffer_cls, value).map(|b| b.buf.clone());
    if buf.is_none() && value != NULL && value != UNDEF {
        throw_type_error(
            g,
            "Failed to set the 'buffer' property on 'ConvolverNode': The provided value is not of \
             type 'AudioBuffer'.",
        );
        return Err(());
    }
    if let Some(b) = &buf {
        let rate = {
            let gl = glob().lock().unwrap();
            gl.nodes
                .get(&h)
                .and_then(|r| gl.engines.get(&r.engine))
                .map_or(b.sample_rate, |e| e.sample_rate)
        };
        let refused = if !matches!(b.data.len(), 1 | 2 | 4) {
            Some(format!(
                "The buffer must have 1, 2, or 4 channels, not {}.",
                b.data.len()
            ))
        } else if b.sample_rate != rate {
            Some(format!(
                "The buffer sample rate of {} does not match the context rate of {rate} Hz.",
                b.sample_rate
            ))
        } else {
            None
        };
        if let Some(m) = refused {
            super::super::domexception::throw(
                g,
                "NotSupportedError",
                &format!("Failed to set the 'buffer' property on 'ConvolverNode': {m}"),
            );
            return Err(());
        }
    }
    // The spectra are computed here, outside the graph lock.
    sh.set_buffer(buf.clone());
    {
        let mut gl = glob().lock().unwrap();
        let gl = &mut *gl;
        if let Some(rec) = gl.nodes.get_mut(&h) {
            rec.buffer = buf;
            let eh = rec.engine;
            graph::resync(&mut gl.engines, &mut gl.nodes, eh);
        }
    }
    define(
        g,
        slots_of(g, node),
        "buffer",
        if is_cell(value) { value } else { NULL },
        0,
    );
    Ok(())
}

unsafe fn make_convolver(g: *mut c_void, engine: u64, proto: u64) -> u64 {
    let params = Params::Convolver(ArcGc::new(ConvShared::default()));
    let node = create_node_on(g, engine, params, proto, &[]);
    if is_cell(node) {
        define(g, slots_of(g, node), "buffer", NULL, 0);
    }
    node
}

#[crate::host_fn]
pub(super) unsafe fn create_convolver(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(engine) = this_engine_handle(g, cf) else {
        return throw_type_error(g, "AudioContext.createConvolver on wrong this");
    };
    make_convolver(g, engine, classes(g).fx.convolver.proto)
}

#[crate::host_fn]
unsafe fn convolver_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let iface = "ConvolverNode";
    let Ok(engine) = context_arg(g, cf, iface) else {
        return UNDEF;
    };
    let Ok(dict) = options_arg(g, cf, iface) else {
        return UNDEF;
    };
    let Ok(buffer) = member(g, dict, "buffer") else {
        return UNDEF;
    };
    let Ok(raw) = member(g, dict, "disableNormalization") else {
        return UNDEF;
    };
    let proto = proto_for(g, cf, classes(g).fx.convolver.proto);
    let node = make_convolver(g, engine, proto);
    if !is_cell(node) || apply_node_options(g, node, dict, iface).is_err() {
        return UNDEF;
    }
    if let Some((_, sh)) = convolver_of(g, node) {
        sh.set_normalize(!to_bool(g, raw));
    }
    if buffer != UNDEF && buffer != NULL && set_response(g, node, buffer).is_err() {
        return UNDEF;
    }
    node
}

#[crate::host_fn]
unsafe fn convolver_get_buffer(g: *mut c_void, cf: *mut c_void) -> u64 {
    if convolver_of(g, cf_this(cf)).is_none() {
        return throw_type_error(g, "ConvolverNode.buffer on wrong this");
    }
    get(g, slots_of(g, cf_this(cf)), "buffer")
}

#[crate::host_fn]
unsafe fn convolver_set_buffer(g: *mut c_void, cf: *mut c_void) -> u64 {
    let _ = set_response(g, cf_this(cf), cf_arg(cf, 0));
    UNDEF
}

#[crate::host_fn]
unsafe fn convolver_get_normalize(g: *mut c_void, cf: *mut c_void) -> u64 {
    match convolver_of(g, cf_this(cf)) {
        Some((_, sh)) => enc_bool(sh.normalize()),
        None => throw_type_error(g, "ConvolverNode.normalize on wrong this"),
    }
}

#[crate::host_fn]
unsafe fn convolver_set_normalize(g: *mut c_void, cf: *mut c_void) -> u64 {
    match convolver_of(g, cf_this(cf)) {
        Some((_, sh)) => {
            sh.set_normalize(to_bool(g, cf_arg(cf, 0)));
            UNDEF
        }
        None => throw_type_error(g, "ConvolverNode.normalize on wrong this"),
    }
}

// ---------------------------------------------------------------------------
// PeriodicWave
// ---------------------------------------------------------------------------

/// The wave of `real` and `imag`, either of which may be absent.
unsafe fn make_wave(
    g: *mut c_void,
    proto: u64,
    real: Option<Vec<f32>>,
    imag: Option<Vec<f32>>,
    normalize: bool,
    what: &str,
) -> u64 {
    let (real, imag) = match (real, imag) {
        (None, None) => (vec![0.0, 0.0], vec![0.0, 1.0]),
        (Some(r), None) => {
            let zeros = vec![0.0; r.len()];
            (r, zeros)
        }
        (None, Some(i)) => (vec![0.0; i.len()], i),
        (Some(r), Some(i)) => (r, i),
    };
    let refused = if real.len() != imag.len() {
        Some(format!(
            "the length of the real array ({}) and of the imaginary array ({}) must match.",
            real.len(),
            imag.len()
        ))
    } else if real.len() < 2 {
        Some(format!(
            "the arrays need at least 2 values, not {}.",
            real.len()
        ))
    } else {
        None
    };
    if let Some(m) = refused {
        return super::super::domexception::throw(g, "IndexSizeError", &format!("{what}: {m}"));
    }
    let table = ArcGc::new(WaveTable::new(&real, &imag, normalize));
    host_create(g, classes(g).fx.wave_cls, proto, WaveData { table })
}

#[crate::host_fn]
pub(super) unsafe fn create_periodic_wave(g: *mut c_void, cf: *mut c_void) -> u64 {
    if this_engine_handle(g, cf).is_none() {
        return throw_type_error(g, "AudioContext.createPeriodicWave on wrong this");
    }
    let (Ok(real), Ok(imag)) = (floats(g, cf_arg(cf, 0)), floats(g, cf_arg(cf, 1))) else {
        return UNDEF;
    };
    let (Some(real), Some(imag)) = (real, imag) else {
        return throw_type_error(
            g,
            "Failed to execute 'createPeriodicWave' on 'BaseAudioContext': 2 arguments required.",
        );
    };
    let constraints = cf_arg(cf, 2);
    let disable = if is_cell(constraints) {
        let Ok(v) = try_get(g, constraints, "disableNormalization") else {
            return UNDEF;
        };
        to_bool(g, v)
    } else {
        false
    };
    make_wave(
        g,
        classes(g).fx.wave.proto,
        Some(real),
        Some(imag),
        !disable,
        "Failed to execute 'createPeriodicWave' on 'BaseAudioContext'",
    )
}

#[crate::host_fn]
unsafe fn wave_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let iface = "PeriodicWave";
    if context_arg(g, cf, iface).is_err() {
        return UNDEF;
    }
    let Ok(dict) = options_arg(g, cf, iface) else {
        return UNDEF;
    };
    let Ok(disable) = member(g, dict, "disableNormalization") else {
        return UNDEF;
    };
    let Ok(imag) = member(g, dict, "imag") else {
        return UNDEF;
    };
    let Ok(real) = member(g, dict, "real") else {
        return UNDEF;
    };
    let (Ok(real), Ok(imag)) = (floats(g, real), floats(g, imag)) else {
        return UNDEF;
    };
    let proto = proto_for(g, cf, classes(g).fx.wave.proto);
    make_wave(
        g,
        proto,
        real,
        imag,
        !to_bool(g, disable),
        "Failed to construct 'PeriodicWave'",
    )
}

/// `OscillatorNode.setPeriodicWave(wave)`.
#[crate::host_fn]
pub(super) unsafe fn osc_set_periodic_wave(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(h) = this_node_handle(g, cf) else {
        return throw_type_error(g, "OscillatorNode.setPeriodicWave on wrong this");
    };
    let Some(wave) = host_data::<WaveData>(g, classes(g).fx.wave_cls, cf_arg(cf, 0)) else {
        return throw_type_error(
            g,
            "Failed to execute 'setPeriodicWave' on 'OscillatorNode': parameter 1 is not of type \
             'PeriodicWave'.",
        );
    };
    let gl = glob().lock().unwrap();
    if let Some(Params::Osc(sh)) = gl.nodes.get(&h).map(|r| &r.params) {
        sh.set_periodic_wave(ArcGc::clone(&wave.table));
    }
    UNDEF
}

pub(super) unsafe fn install(g: *mut c_void, parent: u64) -> Classes {
    let delay = define_class(
        g,
        &ClassSpec {
            name: "DelayNode",
            arity: 1,
            construct: Some(delay_construct),
            parent_proto: parent,
            methods: &[],
            getters: &[("delayTime", get_slot_delay_time)],
            accessors: &[],
        },
    );
    let shaper = define_class(
        g,
        &ClassSpec {
            name: "WaveShaperNode",
            arity: 1,
            construct: Some(shaper_construct),
            parent_proto: parent,
            methods: &[],
            getters: &[],
            accessors: &[
                ("curve", shaper_get_curve, shaper_set_curve),
                ("oversample", shaper_get_oversample, shaper_set_oversample),
            ],
        },
    );
    let compressor = define_class(
        g,
        &ClassSpec {
            name: "DynamicsCompressorNode",
            arity: 1,
            construct: Some(compressor_construct),
            parent_proto: parent,
            methods: &[],
            getters: &[
                ("threshold", get_slot_threshold),
                ("knee", get_slot_knee),
                ("ratio", get_slot_ratio),
                ("attack", get_slot_attack),
                ("release", get_slot_release),
                ("reduction", compressor_get_reduction),
            ],
            accessors: &[],
        },
    );
    let convolver = define_class(
        g,
        &ClassSpec {
            name: "ConvolverNode",
            arity: 1,
            construct: Some(convolver_construct),
            parent_proto: parent,
            methods: &[],
            getters: &[],
            accessors: &[
                ("buffer", convolver_get_buffer, convolver_set_buffer),
                (
                    "normalize",
                    convolver_get_normalize,
                    convolver_set_normalize,
                ),
            ],
        },
    );
    let wave = define_class(
        g,
        &ClassSpec {
            name: "PeriodicWave",
            arity: 1,
            construct: Some(wave_construct),
            parent_proto: 0,
            methods: &[],
            getters: &[],
            accessors: &[],
        },
    );
    Classes {
        delay,
        shaper,
        compressor,
        convolver,
        wave,
        wave_cls: make_host_class::<WaveData>("PeriodicWave"),
    }
}
