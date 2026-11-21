//! Channels through the graph: each node's computed channels, the Firewheel width that carries
//! them, the ports (or mixer) each connection takes, and the panners rendered as backend objects.
//!
//! A node's Firewheel width only grows (a node is rebuilt wider when a wider signal reaches it),
//! and is at least 2 for the nodes Firewheel builds stereo, so a stereo graph is wired exactly as
//! before multichannel: straight port pairs, no mixers, no rebuilds.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};

use firewheel::channel_config::NonZeroChannelCount;
use firewheel::collector::ArcGc;
use firewheel::event::NodeEventType;
use firewheel::node::NodeID;
use firewheel::nodes::sampler::SamplerConfig;
use firewheel::nodes::svf::SvfNode;
use firewheel::nodes::volume::VolumeNodeConfig;

use super::effects::{DelayIn, DelayOut, DelayShared, ShaperNode};
use super::mixing::{self, web_mix, Interp, MixNode, Route};
use super::output::{BusCell, RedBackend};
use super::{
    update_spatial, Engine, FirewheelContext, MonitorNode, NodeRec, PannerRoute, Panning, Params,
    ST_SUSPENDED,
};

/// One connection as built in Firewheel.
pub(super) struct Wire {
    src: NodeID,
    dst: NodeID,
    route: Route,
    mixer: Option<NodeID>,
}

impl Wire {
    fn build(fw: &mut FirewheelContext, src: NodeID, dst: NodeID, route: Route) -> Wire {
        let mixer = match &route {
            Route::Ports(ports) => {
                if let Err(e) = fw.connect(src, dst, ports, true) {
                    // Loud: a dropped edge sounds like silence in a graph wired correctly from JS.
                    eprintln!(
                        "[audio] connect failed ({e:?}) — {src:?} → {dst:?} on ports {ports:?}"
                    );
                }
                None
            }
            Route::Mix {
                n_in,
                n_out,
                matrix,
                from,
                to,
            } => {
                let m = fw.add_node(
                    MixNode {
                        n_in: *n_in,
                        n_out: *n_out,
                        matrix: matrix.clone(),
                    },
                    None,
                );
                let ins: Vec<(u32, u32)> = from
                    .iter()
                    .enumerate()
                    .map(|(k, &p)| (p, k as u32))
                    .collect();
                let wired = fw
                    .connect(src, m, &ins, false)
                    .and_then(|_| fw.connect(m, dst, to, true));
                if let Err(e) = wired {
                    eprintln!("[audio] connect failed ({e:?}) — {src:?} → {dst:?} through a channel mixer");
                }
                Some(m)
            }
        };
        Wire {
            src,
            dst,
            route,
            mixer,
        }
    }

    pub(super) fn tear(self, fw: &mut FirewheelContext) {
        match (self.mixer, &self.route) {
            (Some(m), _) => {
                let _ = fw.remove_node(m);
            }
            (None, Route::Ports(ports)) => {
                fw.disconnect(self.src, self.dst, ports);
            }
            (None, Route::Mix { .. }) => {}
        }
    }

    #[cfg(test)]
    pub(super) fn route(&self) -> &Route {
        &self.route
    }

    fn is(&self, src: NodeID, dst: NodeID, route: &Route) -> bool {
        self.src == src && self.dst == dst && self.route == *route
    }
}

/// The objects of the output backend an engine renders through.
pub(super) struct Objects {
    pub label: String,
    pub bus: Option<Arc<BusCell>>,
    /// Per slot, the panner (node handle) rendering through it; 0 when free.
    pub owners: Vec<u64>,
    /// The bus's `lost_count` when the objects were last assigned.
    pub lost_seen: u32,
}

impl Objects {
    /// Whether the backend gave up an object since the last assignment.
    pub fn lost_changed(&self) -> bool {
        self.bus
            .as_ref()
            .and_then(|b| b.get())
            .is_some_and(|b| b.lost_count() != self.lost_seen)
    }
}

/// The most channels a node's input takes.
fn cap(rec: &NodeRec) -> u32 {
    match rec.params {
        Params::Svf(_) => 8,
        Params::Pan(_)
        | Params::Spatial(_)
        | Params::Native { .. }
        | Params::Custom { .. }
        | Params::Worklet { .. }
        | Params::MediaDest(_)
        | Params::Compressor(_)
        | Params::Convolver(_)
        | Params::Iir(_) => 2,
        Params::Splitter(n) => n,
        Params::Merger { .. } => 1,
        _ => mixing::MAX_CHANNELS,
    }
}

fn source_width(src: &NodeRec, lout: u32) -> u32 {
    match src.params {
        Params::Splitter(_) => 1,
        _ => lout,
    }
}

fn in_width(
    h: u64,
    nodes: &HashMap<u64, NodeRec>,
    incoming: &HashMap<u64, Vec<u64>>,
    memo: &mut HashMap<u64, u32>,
    stack: &mut Vec<u64>,
) -> u32 {
    let rec = &nodes[&h];
    let mut widest = 0;
    for &src in incoming.get(&h).into_iter().flatten() {
        if let Some(s) = nodes.get(&src) {
            let w = out_width(src, nodes, incoming, memo, stack);
            widest = widest.max(source_width(s, w));
        }
    }
    mixing::computed_channels(rec.ch_mode, rec.ch_count, widest).clamp(1, cap(rec))
}

fn out_width(
    h: u64,
    nodes: &HashMap<u64, NodeRec>,
    incoming: &HashMap<u64, Vec<u64>>,
    memo: &mut HashMap<u64, u32>,
    stack: &mut Vec<u64>,
) -> u32 {
    if let Some(&w) = memo.get(&h) {
        return w;
    }
    if stack.contains(&h) {
        return 2;
    }
    stack.push(h);
    let rec = &nodes[&h];
    let w = match &rec.params {
        Params::Gain(_)
        | Params::Svf(_)
        | Params::Analyser { .. }
        | Params::Delay { .. }
        | Params::Shaper(_)
        | Params::Compressor(_)
        | Params::Iir(_) => in_width(h, nodes, incoming, memo, stack),
        // Mono only for a mono input through a mono response (or none).
        Params::Convolver(_) => {
            let response = rec.buffer.as_ref().map_or(1, |b| b.data.len());
            if response > 1 {
                2
            } else {
                in_width(h, nodes, incoming, memo, stack)
            }
        }
        Params::Dest | Params::MediaDest(_) => 0,
        Params::Sampler(_) => rec
            .buffer
            .as_ref()
            .map(|b| b.data.len() as u32)
            .unwrap_or(1)
            .clamp(1, rec.out_ch.max(2)),
        Params::Osc(_) | Params::Splitter(_) => 1,
        Params::PcmStream(cfg) => cfg.channels.max(1),
        Params::Merger { n, .. } => *n,
        Params::Pan(_)
        | Params::Spatial(_)
        | Params::Native { .. }
        | Params::Custom { .. }
        | Params::Worklet { .. }
        | Params::Passthrough => 2,
    };
    stack.pop();
    memo.insert(h, w);
    w
}

/// The channels a connection delivers into `dst`.
fn target(dst: &NodeRec) -> u32 {
    match dst.params {
        // VolumePanNode pans a stereo pair: a mono input plays on both sides, as it always has.
        Params::Pan(_) => 2,
        _ => dst.lin,
    }
}

pub(super) fn plan(src: &NodeRec, output: u32, dst: &NodeRec, input: u32) -> Route {
    let splitter = matches!(src.params, Params::Splitter(_));
    let n_in = if splitter { 1 } else { src.lout.max(1) };
    let port = move |c: u32| if splitter { output } else { c };
    match &dst.params {
        // Input j is the port pair (2j, 2j+1); the merger's processor makes it mono.
        Params::Merger { discrete, .. } => {
            let discrete = discrete.load(Ordering::Relaxed);
            let m = if n_in > 2 && !discrete {
                let both = vec![(0, 0, 1.0), (0, 1, 1.0)];
                mixing::compose(&web_mix(n_in, 1, Interp::Speakers), &both)
            } else {
                web_mix(
                    n_in,
                    2,
                    if discrete {
                        Interp::Discrete
                    } else {
                        Interp::Speakers
                    },
                )
            };
            mixing::route(m, n_in, 2, port, |o| vec![2 * input + o])
        }
        _ => {
            let n = target(dst);
            mixing::route(web_mix(n_in, n, dst.ch_interp), n_in, n, port, |o| vec![o])
        }
    }
}

fn advise_once(flag: &'static OnceLock<()>, msg: &str) {
    flag.get_or_init(|| eprintln!("[audio] advisory: {msg}"));
}

/// The two Firewheel nodes of a `DelayNode`, `width` channels wide: (writer, reader).
pub(super) fn add_delay(
    fw: &mut FirewheelContext,
    shared: &ArcGc<DelayShared>,
    width: u32,
) -> (NodeID, NodeID) {
    let writer = fw.add_node(
        DelayIn {
            shared: ArcGc::clone(shared),
            channels: width,
        },
        None,
    );
    let reader = fw.add_node(
        DelayOut {
            shared: ArcGc::clone(shared),
            channels: width,
        },
        None,
    );
    (writer, reader)
}

/// The `DelayNode`s of `handles` that are in a cycle of connections.
fn delays_in_cycles(nodes: &HashMap<u64, NodeRec>, handles: &[u64]) -> HashSet<u64> {
    let mut found = HashSet::new();
    for &h in handles {
        if !matches!(nodes[&h].params, Params::Delay { .. }) {
            continue;
        }
        let mut seen = HashSet::new();
        let mut todo: Vec<u64> = nodes[&h].outs.iter().map(|c| c.dst).collect();
        while let Some(n) = todo.pop() {
            if n == h {
                found.insert(h);
                break;
            }
            if seen.insert(n) {
                todo.extend(
                    nodes
                        .get(&n)
                        .into_iter()
                        .flat_map(|r| &r.outs)
                        .map(|c| c.dst),
                );
            }
        }
    }
    found
}

/// Rebuild `rec`'s Firewheel node `width` ports wide. Its edges go with the old node; `resync`
/// wires it again.
fn rebuild(eng: &mut Engine, rec: &mut NodeRec, width: u32) {
    let Some(channels) = NonZeroChannelCount::new(width) else {
        return;
    };
    let old = rec.id;
    if let Params::Delay { shared, writer } = &mut rec.params {
        let wider = ArcGc::new(DelayShared::new(
            width,
            shared.max_seconds,
            shared.seconds(),
            eng.sample_rate,
        ));
        let (w, r) = add_delay(&mut eng.fw, &wider, width);
        let _ = eng.fw.remove_node(old);
        let _ = eng.fw.remove_node(*writer);
        *shared = wider;
        *writer = w;
        rec.id = r;
        rec.ordered = false;
        rec.out_ch = width;
        rec.in_ch = width;
        return;
    }
    let new = match &rec.params {
        Params::Shaper(sh) => eng.fw.add_node(
            ShaperNode {
                shared: ArcGc::clone(sh),
                channels: width,
            },
            None,
        ),
        Params::Gain(p) => eng.fw.add_node(*p, Some(VolumeNodeConfig { channels })),
        Params::Dest => eng
            .fw
            .add_node(eng.master_params, Some(VolumeNodeConfig { channels })),
        Params::Analyser { ring, .. } => eng.fw.add_node(
            MonitorNode {
                ring: firewheel::collector::ArcGc::clone(ring),
                channels: width,
            },
            None,
        ),
        Params::Svf(p) => eng.fw.add_node(
            SvfNode::<8> {
                filter_type: p.filter_type,
                cutoff_hz: p.cutoff_hz,
                q_factor: p.q_factor,
                gain: p.gain,
                enabled: p.enabled,
                smooth_seconds: p.smooth_seconds,
                coeff_update_factor: p.coeff_update_factor,
            },
            None,
        ),
        Params::Sampler(s) => eng.fw.add_node(
            (**s).clone(),
            Some(SamplerConfig {
                channels,
                ..Default::default()
            }),
        ),
        _ => return,
    };
    let _ = eng.fw.remove_node(old);
    if matches!(rec.params, Params::Dest) {
        eng.master = new;
    }
    rec.id = new;
    rec.out_ch = width;
    if !matches!(rec.params, Params::Sampler(_)) {
        rec.in_ch = width;
    }
}

fn needed_width(rec: &NodeRec) -> Option<u32> {
    match &rec.params {
        Params::Gain(_)
        | Params::Analyser { .. }
        | Params::Dest
        | Params::Delay { .. }
        | Params::Shaper(_) => Some(rec.lin.max(2)),
        Params::Svf(_) => Some(if rec.lin > 2 { 8 } else { 2 }),
        Params::Sampler(_) => {
            let ch = rec
                .buffer
                .as_ref()
                .map(|b| b.data.len() as u32)
                .unwrap_or(1)
                .max(2);
            if rec.started && ch > rec.out_ch {
                static WIDE: OnceLock<()> = OnceLock::new();
                advise_once(
                    &WIDE,
                    "an AudioBufferSourceNode was given a buffer wider than stereo after start(); it \
                     plays the first two channels. Set the buffer before start().",
                );
                return None;
            }
            Some(ch)
        }
        _ => None,
    }
}

/// Recompute every node's channels in engine `eh` and bring the Firewheel graph in line: rebuild
/// nodes that must carry more channels, rewire the connections whose route changed, and map the
/// destination onto the output. Run after anything that changes channels: a connection, a channel
/// attribute, a buffer, a node going, the output reopening.
pub(super) fn resync(
    engines: &mut HashMap<u64, Engine>,
    nodes: &mut HashMap<u64, NodeRec>,
    eh: u64,
) {
    let Some(eng) = engines.get_mut(&eh) else {
        return;
    };
    let mut handles: Vec<u64> = nodes
        .iter()
        .filter(|(_, r)| r.engine == eh)
        .map(|(&h, _)| h)
        .collect();
    handles.sort_unstable();

    let mut incoming: HashMap<u64, Vec<u64>> = HashMap::new();
    for &h in &handles {
        for c in &nodes[&h].outs {
            incoming.entry(c.dst).or_default().push(h);
        }
    }
    let mut memo = HashMap::new();
    let mut stack = Vec::new();
    let widths: Vec<(u64, u32, u32)> = handles
        .iter()
        .map(|&h| {
            let lout = out_width(h, nodes, &incoming, &mut memo, &mut stack);
            (
                h,
                in_width(h, nodes, &incoming, &mut memo, &mut stack),
                lout,
            )
        })
        .collect();

    for (h, lin, lout) in widths {
        let rec = nodes.get_mut(&h).unwrap();
        rec.lin = lin;
        rec.lout = lout;
        if let Params::Svf(_) = rec.params {
            if rec.lin == 8 && rec.ch_mode == mixing::CountMode::Max {
                static WIDE: OnceLock<()> = OnceLock::new();
                advise_once(
                    &WIDE,
                    "a BiquadFilterNode filters at most 8 channels here; the channels after the 8th are dropped.",
                );
            }
        }
        if let Some(w) = needed_width(rec) {
            if w > rec.out_ch.max(rec.in_ch) {
                rebuild(eng, rec, w);
            }
        }
        match &rec.params {
            Params::Analyser { ring, .. } => ring.set_mix(&web_mix(rec.lin, 1, rec.ch_interp)),
            Params::Convolver(sh) => sh.mono.store(rec.lin == 1, Ordering::Relaxed),
            _ => {}
        }
    }

    // A delay that is now in a cycle loses its writer → reader edge before the connection
    // closing the cycle is built; one that left its cycle gets the edge back after.
    let cyclic = delays_in_cycles(nodes, &handles);
    for h in &cyclic {
        let rec = nodes.get_mut(h).unwrap();
        if let (true, Params::Delay { writer, .. }) = (rec.ordered, &rec.params) {
            eng.fw.disconnect(*writer, rec.id, &[(0, 0)]);
            rec.ordered = false;
        }
    }

    let mut jobs = Vec::new();
    for &h in &handles {
        let src = &nodes[&h];
        for c in &src.outs {
            if let Some(dst) = nodes.get(&c.dst) {
                jobs.push((
                    h,
                    *c,
                    src.id,
                    dst.sink_id(),
                    plan(src, c.output, dst, c.input),
                ));
            }
        }
    }
    for (h, conn, sid, did, route) in jobs {
        let rec = nodes.get_mut(&h).unwrap();
        if rec.wires.get(&conn).is_some_and(|w| w.is(sid, did, &route)) {
            continue;
        }
        if let Some(w) = rec.wires.remove(&conn) {
            w.tear(&mut eng.fw);
        }
        rec.wires
            .insert(conn, Wire::build(&mut eng.fw, sid, did, route));
    }
    for &h in &handles {
        let rec = nodes.get_mut(&h).unwrap();
        if let (false, Params::Delay { writer, .. }) = (rec.ordered, &rec.params) {
            if !cyclic.contains(&h) {
                rec.ordered = eng.fw.connect(*writer, rec.id, &[(0, 0)], true).is_ok();
            }
        }
    }

    let (c, interp) = handles
        .iter()
        .map(|h| &nodes[h])
        .find(|r| matches!(r.params, Params::Dest))
        .map(|d| (d.lin, d.ch_interp))
        .unwrap_or((2, Interp::Speakers));
    let d = eng.device_channels.max(1);
    let master = eng.master;
    let out = eng.fw.graph_out_node_id();
    let route = mixing::route(
        mixing::device_mix(c, interp, d, eng.device_mask),
        c,
        d,
        |x| x,
        |o| vec![o],
    );
    if !eng
        .out_wire
        .as_ref()
        .is_some_and(|w| w.is(master, out, &route))
    {
        if let Some(w) = eng.out_wire.take() {
            w.tear(&mut eng.fw);
        }
        eng.out_wire = Some(Wire::build(&mut eng.fw, master, out, route));
    }
    object_sends(eng, nodes, eh);
    if let Some(tap) = eng.ref_tap {
        let route = mixing::route(web_mix(c, 2, Interp::Speakers), c, 2, |x| x, |o| vec![o]);
        if !eng
            .tap_wire
            .as_ref()
            .is_some_and(|w| w.is(master, tap, &route))
        {
            if let Some(w) = eng.tap_wire.take() {
                w.tear(&mut eng.fw);
            }
            eng.tap_wire = Some(Wire::build(&mut eng.fw, master, tap, route));
        }
    }
}

/// Take the wires of a node that is going away down with it.
pub(super) fn unwire(eng: &mut Engine, rec: &mut NodeRec) {
    for (_, w) in rec.wires.drain() {
        w.tear(&mut eng.fw);
    }
}

/// Read what the stream that just started is: its channels, and the backend's objects.
pub(super) fn adopt_output(eng: &mut Engine) {
    let (channels, mask, objects, refused) = match eng.fw.active_backend() {
        Some(RedBackend::Cpal(c)) => (c.channels, 0, None, c.refused.clone()),
        Some(RedBackend::Embedder(e)) => {
            let slots = e.bus.as_ref().and_then(|b| b.get()).map_or(0, |b| b.len());
            let objects = Objects {
                label: e.label.clone(),
                bus: e.bus.clone(),
                owners: vec![0; slots],
                lost_seen: 0,
            };
            (e.channels, e.channel_mask, Some(objects), None)
        }
        None => (2, 0, None, None),
    };
    if let (Some(reason), Some(model)) = (&refused, super::output::object_model()) {
        if eng.refused.as_deref() != Some(reason.as_str()) {
            eprintln!(
                "[audio] the '{model}' output is not available ({reason}); PannerNodes set to '{model}' \
                 render with HRTF instead"
            );
        }
    }
    eng.refused = refused;
    eng.objects = objects;
    eng.device_channels = channels.clamp(1, mixing::MAX_CHANNELS);
    eng.device_mask = mask;
}

/// Free every object slot before the stream that owns them goes.
pub(super) fn drop_objects(eng: &mut Engine) {
    if let Some(o) = eng.objects.take() {
        if let Some(bus) = o.bus.as_ref().and_then(|b| b.get()) {
            bus.release_all();
        }
    }
}

fn route_event(bus: Option<Arc<BusCell>>, slot: usize, owner: u64) -> NodeEventType {
    NodeEventType::custom(PannerRoute { bus, slot, owner })
}

/// Give each panner in the object model a free object of the output, in creation order; the
/// rest render with HRTF. Releases the objects of panners that left the model.
pub(super) fn assign_objects(eng: &mut Engine, nodes: &mut HashMap<u64, NodeRec>, eh: u64) {
    let mut handles: Vec<u64> = nodes
        .iter()
        .filter(|(_, r)| r.engine == eh && matches!(r.params, Params::Spatial(_)))
        .map(|(&h, _)| h)
        .collect();
    handles.sort_unstable();

    if let Some(o) = eng.objects.as_mut() {
        let bus = o.bus.as_ref().and_then(|b| b.get());
        if let Some(bus) = bus {
            let lost = bus.lost_count();
            if lost != o.lost_seen {
                o.lost_seen = lost;
                static LOST: OnceLock<()> = OnceLock::new();
                advise_once(
                    &LOST,
                    &format!(
                        "the '{}' output ran out of objects; the PannerNodes that lost theirs render with HRTF",
                        o.label
                    ),
                );
            }
        }
        for (i, owner) in o.owners.iter_mut().enumerate() {
            let held = !bus.is_some_and(|b| b.is_lost(i))
                && nodes
                    .get(owner)
                    .is_some_and(|r| r.panning == Panning::Objects && r.object as usize == i + 1);
            if *owner != 0 && !held {
                *owner = 0;
                if let Some(bus) = o.bus.as_ref().and_then(|b| b.get()) {
                    bus.set_owner(i, 0);
                }
            }
        }
    }
    let live = eng.objects.as_ref().and_then(|o| {
        o.bus
            .clone()
            .filter(|b| b.get().is_some())
            .map(|b| (b, o.owners.len()))
    });
    for &h in &handles {
        let rec = nodes.get_mut(&h).unwrap();
        let slot = rec.object as usize;
        let keep = slot != 0
            && rec.panning == Panning::Objects
            && eng
                .objects
                .as_ref()
                .is_some_and(|o| o.owners.get(slot - 1) == Some(&h));
        if slot != 0 && !keep {
            if let Some(o) = eng.objects.as_mut() {
                if let Some(owner) = o.owners.get_mut(slot - 1).filter(|o| **o == h) {
                    *owner = 0;
                    if let Some(bus) = o.bus.as_ref().and_then(|b| b.get()) {
                        bus.set_owner(slot - 1, 0);
                    }
                }
            }
            rec.object = 0;
            eng.fw.queue_event_for(rec.id, route_event(None, 0, 0));
            update_spatial(eng, rec);
        }
    }
    let Some((cell, slots)) = live else { return };
    for &h in &handles {
        let rec = nodes.get_mut(&h).unwrap();
        if rec.panning != Panning::Objects || rec.object != 0 {
            continue;
        }
        let o = eng.objects.as_mut().unwrap();
        let bus = cell.get();
        let Some(i) = o
            .owners
            .iter()
            .enumerate()
            .position(|(i, &x)| x == 0 && !bus.is_some_and(|b| b.is_lost(i)))
        else {
            static FULL: OnceLock<()> = OnceLock::new();
            advise_once(
                &FULL,
                &format!(
                    "every one of the output's {slots} objects is in use; the PannerNodes created after \
                     those render with HRTF"
                ),
            );
            break;
        };
        o.owners[i] = h;
        if let Some(bus) = cell.get() {
            bus.set_owner(i, h);
        }
        rec.object = i as u32 + 1;
        eng.fw
            .queue_event_for(rec.id, route_event(Some(Arc::clone(&cell)), i, h));
        update_spatial(eng, rec);
    }
    object_sends(eng, nodes, eh);
}

/// An object panner's sound goes to the backend, past the nodes after it. Of those, the gains carry
/// over: each object gets the loudest product of `GainNode` gains on a path to the destination, 0
/// without one (and while the context is suspended). Other nodes on the path are skipped.
pub(super) fn object_sends(eng: &mut Engine, nodes: &mut HashMap<u64, NodeRec>, eh: u64) {
    let panners: Vec<u64> = nodes
        .iter()
        .filter(|(_, r)| r.engine == eh && r.object != 0)
        .map(|(&h, _)| h)
        .collect();
    if panners.is_empty() {
        return;
    }
    let master = if eng.state == ST_SUSPENDED { 0.0 } else { 1.0 };
    let mut memo: HashMap<u64, f32> = HashMap::new();
    for h in panners {
        let send = nodes[&h]
            .outs
            .iter()
            .map(|c| reach(nodes, c.dst, master, &mut memo, &mut Vec::new()))
            .fold(0.0, f32::max);
        let rec = nodes.get_mut(&h).unwrap();
        if rec.send != send {
            rec.send = send;
            update_spatial(eng, rec);
        }
    }
}

fn reach(
    nodes: &HashMap<u64, NodeRec>,
    h: u64,
    master: f32,
    memo: &mut HashMap<u64, f32>,
    path: &mut Vec<u64>,
) -> f32 {
    if let Some(&v) = memo.get(&h) {
        return v;
    }
    let Some(rec) = nodes.get(&h) else { return 0.0 };
    if matches!(rec.params, Params::Dest) {
        return master;
    }
    if path.contains(&h) {
        return 0.0;
    }
    path.push(h);
    let own = if matches!(rec.params, Params::Gain(_)) {
        rec.gain.abs()
    } else {
        1.0
    };
    let best = rec
        .outs
        .iter()
        .map(|c| reach(nodes, c.dst, master, memo, path))
        .fold(0.0, f32::max);
    path.pop();
    let v = own * best;
    memo.insert(h, v);
    v
}
