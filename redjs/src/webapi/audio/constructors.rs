//! `new GainNode(context, options)` and the other nodes a factory used to be the only way to: the
//! node is made as its factory makes it, then each option goes through the attribute or the
//! `AudioParam` of the same name, so a constructor refuses what the setter refuses.

use super::effect_nodes::{apply_node_options, context_arg, member, options_arg, proto_for};
use super::*;

/// What an options dictionary may carry besides the `AudioNodeOptions` members.
struct Options {
    /// Members that set the `value` of the `AudioParam` of the same name.
    params: &'static [&'static str],
    /// Members that set the attribute of the same name.
    attributes: &'static [&'static str],
}

/// `target[name] = value` with the setter's exception rethrown.
unsafe fn assign(g: *mut c_void, target: u64, name: &str, value: u64) -> Result<(), ()> {
    let set = get(g, get(g, glue::rjs_global(g), "Reflect"), "set");
    match call(g, set, UNDEF, &[target, js_string(g, name), value]) {
        Ok(_) => Ok(()),
        Err(thrown) => {
            glue::rjs_throw(g, thrown);
            Err(())
        }
    }
}

unsafe fn construct(
    g: *mut c_void,
    cf: *mut c_void,
    iface: &str,
    fallback_proto: u64,
    make: unsafe fn(*mut c_void, u64, u64) -> u64,
    options: &Options,
) -> u64 {
    let Ok(engine) = context_arg(g, cf, iface) else {
        return UNDEF;
    };
    let Ok(dict) = options_arg(g, cf, iface) else {
        return UNDEF;
    };
    let node = make(g, engine, proto_for(g, cf, fallback_proto));
    if !is_cell(node) || apply_node_options(g, node, dict, iface).is_err() {
        return UNDEF;
    }
    for &name in options.attributes {
        let Ok(value) = member(g, dict, name) else {
            return UNDEF;
        };
        if value != UNDEF && assign(g, node, name, value).is_err() {
            return UNDEF;
        }
    }
    for &name in options.params {
        let Ok(value) = member(g, dict, name) else {
            return UNDEF;
        };
        if value != UNDEF && assign(g, get(g, node, name), "value", value).is_err() {
            return UNDEF;
        }
    }
    node
}

macro_rules! constructor {
    ($fn_name:ident, $iface:literal, $class:ident, $make:ident, $params:expr, $attributes:expr) => {
        #[crate::host_fn]
        pub(super) unsafe fn $fn_name(g: *mut c_void, cf: *mut c_void) -> u64 {
            const OPTIONS: Options = Options {
                params: &$params,
                attributes: &$attributes,
            };
            construct(g, cf, $iface, classes(g).$class.proto, $make, &OPTIONS)
        }
    };
}

constructor!(gain_construct, "GainNode", gain, make_gain, ["gain"], []);
constructor!(
    stereo_panner_construct,
    "StereoPannerNode",
    stereo,
    make_stereo_panner,
    ["pan"],
    []
);
constructor!(
    panner_construct,
    "PannerNode",
    panner,
    make_panner,
    [
        "positionX",
        "positionY",
        "positionZ",
        "orientationX",
        "orientationY",
        "orientationZ"
    ],
    [
        "panningModel",
        "distanceModel",
        "refDistance",
        "maxDistance",
        "rolloffFactor",
        "coneInnerAngle",
        "coneOuterAngle",
        "coneOuterGain"
    ]
);
constructor!(
    biquad_construct,
    "BiquadFilterNode",
    biquad,
    make_biquad,
    ["frequency", "Q", "gain", "detune"],
    ["type"]
);
constructor!(
    analyser_construct,
    "AnalyserNode",
    analyser,
    make_analyser,
    [],
    [
        "fftSize",
        "maxDecibels",
        "minDecibels",
        "smoothingTimeConstant"
    ]
);
constructor!(
    buffer_source_construct,
    "AudioBufferSourceNode",
    source,
    make_buffer_source,
    ["playbackRate", "detune"],
    ["buffer", "loop", "loopStart", "loopEnd"]
);

/// `periodicWave` takes the place of `type`: with a wave the type is "custom", which the `type`
/// setter refuses.
#[crate::host_fn]
pub(super) unsafe fn oscillator_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let iface = "OscillatorNode";
    let Ok(dict) = options_arg(g, cf, iface) else {
        return UNDEF;
    };
    let Ok(wave) = member(g, dict, "periodicWave") else {
        return UNDEF;
    };
    let with_wave = wave != UNDEF && wave != NULL;
    let options = Options {
        params: &["frequency", "detune"],
        attributes: if with_wave { &[] } else { &["type"] },
    };
    let node = construct(
        g,
        cf,
        iface,
        classes(g).osc.proto,
        make_oscillator,
        &options,
    );
    if !is_cell(node) || !with_wave {
        return node;
    }
    match call(g, get(g, node, "setPeriodicWave"), node, &[wave]) {
        Ok(_) => node,
        Err(thrown) => {
            glue::rjs_throw(g, thrown);
            UNDEF
        }
    }
}

/// `new AudioBuffer({ length, sampleRate, numberOfChannels })`.
#[crate::host_fn]
pub(super) unsafe fn buffer_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let options = cf_arg(cf, 0);
    if !super::super::blob::is_object(g, options) {
        return throw_type_error(
            g,
            "Failed to construct 'AudioBuffer': 1 argument required, an AudioBufferOptions dictionary.",
        );
    }
    let mut numbers = [1.0, f64::NAN, f64::NAN];
    for (slot, name) in numbers
        .iter_mut()
        .zip(["numberOfChannels", "length", "sampleRate"])
    {
        let Ok(value) = try_get(g, options, name) else {
            return UNDEF;
        };
        if value == UNDEF {
            continue;
        }
        let Ok(number) = to_number(g, value) else {
            return UNDEF;
        };
        *slot = number;
    }
    let [channels, length, rate] = numbers;
    if length.is_nan() || rate.is_nan() {
        return throw_type_error(
            g,
            "Failed to construct 'AudioBuffer': 'length' and 'sampleRate' are required.",
        );
    }
    let refused = if !(1.0..=32.0).contains(&channels) {
        Some(format!(
            "The number of channels provided ({channels}) is outside the range [1, 32]."
        ))
    } else if !(length >= 1.0 && length <= u32::MAX as f64) {
        Some(format!(
            "The number of frames provided ({length}) is less than or equal to the minimum bound (0)."
        ))
    } else if !(3000.0..=768_000.0).contains(&rate) {
        Some(format!(
            "The sample rate provided ({rate}) is outside the range [3000, 768000]."
        ))
    } else {
        None
    };
    if let Some(reason) = refused {
        return super::super::domexception::throw(
            g,
            "NotSupportedError",
            &format!("Failed to construct 'AudioBuffer': {reason}"),
        );
    }
    wrap_buffer(
        g,
        Arc::new(PlanarBuffer {
            data: vec![vec![0.0; length as usize]; channels as usize],
            sample_rate: rate as u32,
        }),
    )
}
