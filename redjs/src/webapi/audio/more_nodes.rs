//! The JS side of `ConstantSourceNode` and `IIRFilterNode`.

use super::effect_nodes::{apply_node_options, context_arg, member, options_arg, proto_for};
use super::effects::{IirShared, IIR_MAX};
use super::*;

pub(super) struct Classes {
    pub constant: Class,
    pub iir: Class,
}

// ---------------------------------------------------------------------------
// ConstantSourceNode
// ---------------------------------------------------------------------------

/// An oscillator with no wave: its `frequency` cell carries `offset`, and `start()`, `stop()`
/// and `onended` are the oscillator's.
unsafe fn make_constant(g: *mut c_void, engine: u64, proto: u64, offset: f64) -> u64 {
    let shared = OscShared::default();
    shared.set_wave(Wave::Constant);
    shared.set_frequency(offset as f32);
    let node = create_node_on(
        g,
        engine,
        Params::Osc(ArcGc::new(shared)),
        proto,
        &[("offset", P_FREQ, offset)],
    );
    if is_cell(node) {
        define(g, slots_of(g, node), "onended", NULL, 0);
    }
    node
}

#[crate::host_fn]
pub(super) unsafe fn create_constant_source(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_engine_handle(g, cf) {
        Some(engine) => make_constant(g, engine, classes(g).more.constant.proto, 1.0),
        None => throw_type_error(g, "AudioContext.createConstantSource on wrong this"),
    }
}

#[crate::host_fn]
unsafe fn constant_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let iface = "ConstantSourceNode";
    let Ok(engine) = context_arg(g, cf, iface) else {
        return UNDEF;
    };
    let Ok(dict) = options_arg(g, cf, iface) else {
        return UNDEF;
    };
    let Ok(offset) = member(g, dict, "offset") else {
        return UNDEF;
    };
    let offset = if offset == UNDEF {
        1.0
    } else {
        match to_number(g, offset) {
            Ok(n) if n.is_finite() => n,
            Ok(_) => return throw_type_error(g, "The provided value for 'offset' is non-finite."),
            Err(()) => return UNDEF,
        }
    };
    let proto = proto_for(g, cf, classes(g).more.constant.proto);
    make_constant(g, engine, proto, offset)
}

#[crate::host_fn]
unsafe fn get_slot_offset(g: *mut c_void, cf: *mut c_void) -> u64 {
    get(g, slots_of(g, cf_this(cf)), "offset")
}

// ---------------------------------------------------------------------------
// IIRFilterNode
// ---------------------------------------------------------------------------

/// A `sequence<double>` of coefficients. `Err` has thrown.
unsafe fn coefficients(g: *mut c_void, v: u64, what: &str, name: &str) -> Result<Vec<f64>, ()> {
    if !super::super::blob::is_object(g, v) {
        throw_type_error(g, &format!("{what}: '{name}' is not a sequence."));
        return Err(());
    }
    let len = to_number(g, try_get(g, v, "length")?)?;
    if !(len >= 1.0 && len <= IIR_MAX as f64) {
        super::super::domexception::throw(
            g,
            "NotSupportedError",
            &format!(
                "{what}: the number of {name} coefficients ({len}) is outside the range [1, {IIR_MAX}]."
            ),
        );
        return Err(());
    }
    let mut out = Vec::with_capacity(len as usize);
    for i in 0..len as u32 {
        let n = to_number(g, get_i(g, v, i))?;
        if !n.is_finite() {
            throw_type_error(g, &format!("{what}: a {name} coefficient is non-finite."));
            return Err(());
        }
        out.push(n);
    }
    Ok(out)
}

unsafe fn make_iir(
    g: *mut c_void,
    engine: u64,
    proto: u64,
    feedforward: u64,
    feedback: u64,
    what: &str,
) -> u64 {
    let Ok(feedforward) = coefficients(g, feedforward, what, "feedforward") else {
        return UNDEF;
    };
    let Ok(feedback) = coefficients(g, feedback, what, "feedback") else {
        return UNDEF;
    };
    let refused = if feedforward.iter().all(|&b| b == 0.0) {
        Some("every feedforward coefficient is zero.")
    } else if feedback[0] == 0.0 {
        Some("the first feedback coefficient is zero.")
    } else {
        None
    };
    if let Some(reason) = refused {
        return super::super::domexception::throw(
            g,
            "InvalidStateError",
            &format!("{what}: {reason}"),
        );
    }
    let shared = ArcGc::new(IirShared::new(&feedforward, &feedback));
    create_node_on(g, engine, Params::Iir(shared), proto, &[])
}

#[crate::host_fn]
pub(super) unsafe fn create_iir_filter(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(engine) = this_engine_handle(g, cf) else {
        return throw_type_error(g, "AudioContext.createIIRFilter on wrong this");
    };
    make_iir(
        g,
        engine,
        classes(g).more.iir.proto,
        cf_arg(cf, 0),
        cf_arg(cf, 1),
        "Failed to execute 'createIIRFilter' on 'BaseAudioContext'",
    )
}

#[crate::host_fn]
unsafe fn iir_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let iface = "IIRFilterNode";
    let what = "Failed to construct 'IIRFilterNode'";
    let Ok(engine) = context_arg(g, cf, iface) else {
        return UNDEF;
    };
    let Ok(Some(dict)) = options_arg(g, cf, iface) else {
        return throw_type_error(
            g,
            &format!(
                "{what}: an options dictionary with 'feedforward' and 'feedback' is required."
            ),
        );
    };
    let (Ok(feedforward), Ok(feedback)) = (
        try_get(g, dict, "feedforward"),
        try_get(g, dict, "feedback"),
    ) else {
        return UNDEF;
    };
    let proto = proto_for(g, cf, classes(g).more.iir.proto);
    let node = make_iir(g, engine, proto, feedforward, feedback, what);
    if !is_cell(node) || apply_node_options(g, node, Some(dict), iface).is_err() {
        return UNDEF;
    }
    node
}

/// `getFrequencyResponse(frequencyHz, magResponse, phaseResponse)`: three `Float32Array`s of
/// one length. A frequency outside [0, Nyquist] answers NaN.
#[crate::host_fn]
unsafe fn iir_frequency_response(g: *mut c_void, cf: *mut c_void) -> u64 {
    let what = "Failed to execute 'getFrequencyResponse' on 'IIRFilterNode'";
    let Some(h) = this_node_handle(g, cf) else {
        return throw_type_error(g, "IIRFilterNode.getFrequencyResponse on wrong this");
    };
    let mut arrays = [crate::ffi::RjsTaInfo::default(); 3];
    for (i, info) in arrays.iter_mut().enumerate() {
        if glue::rjs_typedarray_info(g, cf_arg(cf, i as u32), info) != 0 || info.kind != 8 {
            return throw_type_error(
                g,
                &format!("{what}: parameter {} is not of type 'Float32Array'.", i + 1),
            );
        }
    }
    let [hz, magnitude, phase] = arrays;
    if magnitude.length != hz.length || phase.length != hz.length {
        return super::super::domexception::throw(
            g,
            "InvalidAccessError",
            &format!("{what}: the three arrays are not of one length."),
        );
    }
    let (shared, rate) = {
        let gl = glob().lock().unwrap();
        let Some(rec) = gl.nodes.get(&h) else {
            return UNDEF;
        };
        let Params::Iir(shared) = &rec.params else {
            return UNDEF;
        };
        let rate = gl
            .engines
            .get(&rec.engine)
            .map_or(48_000, |e| e.sample_rate);
        (ArcGc::clone(shared), rate as f64)
    };
    if hz.length == 0 || hz.data.is_null() || magnitude.data.is_null() || phase.data.is_null() {
        return UNDEF;
    }
    let n = hz.length as usize;
    let hz = core::slice::from_raw_parts(hz.data as *const f32, n).to_vec();
    let magnitude = core::slice::from_raw_parts_mut(magnitude.data as *mut f32, n);
    let phase = core::slice::from_raw_parts_mut(phase.data as *mut f32, n);
    for (i, f) in hz.into_iter().enumerate() {
        let f = f as f64;
        (magnitude[i], phase[i]) = if (0.0..=rate / 2.0).contains(&f) {
            shared.response(core::f64::consts::TAU * f / rate)
        } else {
            (f32::NAN, f32::NAN)
        };
    }
    UNDEF
}

pub(super) unsafe fn install(g: *mut c_void, parent: u64) -> Classes {
    let constant = define_class(
        g,
        &ClassSpec {
            name: "ConstantSourceNode",
            arity: 1,
            construct: Some(constant_construct),
            parent_proto: parent,
            methods: &[("start", 0, src_start), ("stop", 0, src_stop)],
            getters: &[("offset", get_slot_offset)],
            accessors: &[("onended", src_get_onended, src_set_onended)],
        },
    );
    let iir = define_class(
        g,
        &ClassSpec {
            name: "IIRFilterNode",
            arity: 2,
            construct: Some(iir_construct),
            parent_proto: parent,
            methods: &[("getFrequencyResponse", 3, iir_frequency_response)],
            getters: &[],
            accessors: &[],
        },
    );
    Classes { constant, iir }
}
