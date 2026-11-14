// `MediaStream`, `MediaStreamTrack`, `getUserMedia` and the two Web Audio
// nodes that meet a stream (Media Capture and Streams; Web Audio §1.24, §1.25).
//
// Audio only. The tracks, the devices and the frames are native
// (webapi/mediastream.rs, webapi/audio.rs); this file is the browser-shaped
// surface over them: the classes, the constraint reading, the events.
//
// It is evaluated once per context by `mediastream::install` and returns the
// two hooks the native pump calls: `settle` (a `getUserMedia` open finished)
// and `ended` (a device went away under a live track).
//
// WHAT DIFFERS FROM A BROWSER, ON PURPOSE
//
//  * The prompt is the host's. Whether a microphone opens, and whether the
//    user is asked first, is the capture gate (see the header of
//    mediastream.rs); `open` returns nothing while the host asks, and the
//    answer arrives through `settle` like any open. A refusal is a
//    `NotAllowedError` with Chrome's message ("Permission denied",
//    "Permission dismissed", "Permission denied by system").
//  * `echoCancellation`, `noiseSuppression` and `autoGainControl` default as
//    in Chrome: each is on unless the constraints turn it off. They run only
//    when the embedder installed capture processing (mediastream.rs,
//    "Processing"). `getSettings()` reports what is in effect, so without it
//    all three are `false`, and asking for one explicitly logs an advisory.
//    A processed track is mono.
//  * Video is refused with `NotSupportedError`.
//  * `applyConstraints()` records the constraints and changes nothing.
(function (g, N) {
  "use strict";

  var hooks = { settle: function () {}, ended: function () {} };
  var ET = g.EventTarget;
  var DOMEx = g.DOMException;
  if (typeof ET !== "function" || typeof DOMEx !== "function") return hooks;

  var Ev = g.Event;
  var WR = typeof g.WeakRef === "function" ? g.WeakRef : null;
  var TypeErr = TypeError;

  function named(name, message) {
    return new DOMEx(message, name);
  }

  function tag(C, name) {
    Object.defineProperty(C.prototype, Symbol.toStringTag, { value: name, configurable: true });
  }

  function publish(name, C) {
    tag(C, name);
    Object.defineProperty(g, name, { value: C, writable: true, configurable: true, enumerable: false });
  }

  function method(obj, name, fn) {
    Object.defineProperty(obj, name, { value: fn, writable: true, configurable: true, enumerable: false });
  }

  function uuid() {
    try {
      if (g.crypto && typeof g.crypto.randomUUID === "function") return g.crypto.randomUUID();
    } catch (e) {}
    var h = "";
    for (var i = 0; i < 32; i++) h += ((Math.random() * 16) | 0).toString(16);
    return h.slice(0, 8) + "-" + h.slice(8, 12) + "-4" + h.slice(13, 16) + "-a" + h.slice(17, 20) + "-" + h.slice(20);
  }

  function shallow(o) {
    var out = {};
    if (o && typeof o === "object") for (var k in o) if (o[k] !== undefined) out[k] = o[k];
    return out;
  }

  // `onended = fn`: registered as a listener the first time it is set, so it
  // runs in its place among the listeners, as an event handler attribute does.
  var handlers = new WeakMap();
  function handlerAttr(proto, type) {
    var key = "on" + type;
    Object.defineProperty(proto, key, {
      get: function () {
        var s = handlers.get(this);
        return (s && s[key]) || null;
      },
      set: function (fn) {
        var s = handlers.get(this);
        if (!s) handlers.set(this, (s = {}));
        if (!s["+" + key]) {
          s["+" + key] = true;
          var self = this;
          this.addEventListener(type, function (e) {
            var f = handlers.get(self)[key];
            if (typeof f === "function") return f.call(self, e);
          });
        }
        s[key] = typeof fn === "function" ? fn : null;
      },
      enumerable: true,
      configurable: true,
    });
  }

  // ---------------------------------------------------------------------------
  // OverconstrainedError
  // ---------------------------------------------------------------------------

  var constraintOf = new WeakMap();
  class OverconstrainedError extends DOMEx {
    constructor(constraint, message) {
      super(message === undefined ? "" : String(message), "OverconstrainedError");
      constraintOf.set(this, String(constraint));
    }
    get constraint() {
      if (!constraintOf.has(this)) throw new TypeErr("Illegal invocation");
      return constraintOf.get(this);
    }
  }
  if (typeof g.OverconstrainedError !== "function") publish("OverconstrainedError", OverconstrainedError);

  function failure(name, message) {
    if (name === "OverconstrainedError") return new OverconstrainedError("deviceId", message);
    return named(name, message);
  }

  // ---------------------------------------------------------------------------
  // MediaStreamTrack
  // ---------------------------------------------------------------------------

  var NEW = {};
  var T = new WeakMap(); // track → its state
  var byNative = new Map(); // native id → WeakRef(track), for `ended`

  function trackState(t, what) {
    var s = T.get(t);
    if (!s) throw new TypeErr("Failed to execute '" + what + "' on 'MediaStreamTrack': Illegal invocation");
    return s;
  }

  function remember(nid, t) {
    if (byNative.size > 256) byNative.forEach(function (w, k) { if (!w.deref()) byNative.delete(k); });
    byNative.set(nid, WR ? new WR(t) : { deref: function () { return t; } });
  }

  class MediaStreamTrack extends ET {
    constructor(token, handle, constraints, from) {
      if (token !== NEW) throw new TypeErr("Illegal constructor");
      super();
      var info = N.info(handle);
      T.set(this, {
        h: handle,
        nid: info ? info.id : 0,
        id: uuid(),
        label: info ? info.label : "",
        source: info ? info.kind : "graph",
        enabled: from ? from.enabled : true,
        ended: from ? from.ended : !info || info.ended,
        contentHint: from ? from.contentHint : "",
        constraints: shallow(constraints),
      });
      if (info) remember(info.id, this);
    }
    get kind() { return trackState(this, "kind"), "audio"; }
    get id() { return trackState(this, "id").id; }
    get label() { return trackState(this, "label").label; }
    get enabled() { return trackState(this, "enabled").enabled; }
    set enabled(v) {
      var s = trackState(this, "enabled");
      s.enabled = !!v;
      N.setEnabled(s.h, s.enabled);
    }
    get muted() { return trackState(this, "muted"), false; }
    get readyState() { return trackState(this, "readyState").ended ? "ended" : "live"; }
    get contentHint() { return trackState(this, "contentHint").contentHint; }
    set contentHint(v) {
      var s = trackState(this, "contentHint");
      v = String(v);
      if (v === "" || v === "speech" || v === "speech-recognition" || v === "music") s.contentHint = v;
    }
    stop() {
      var s = trackState(this, "stop");
      if (s.ended) return;
      s.ended = true; // no `ended` event: the page did it (Media Capture §4.3.1)
      N.stop(s.h);
    }
    clone() {
      var s = trackState(this, "clone");
      var h = N.clone(s.h);
      if (!h) throw named("InvalidStateError", "Failed to execute 'clone' on 'MediaStreamTrack': the track is gone.");
      var c = new MediaStreamTrack(NEW, h, s.constraints, s);
      T.get(c).label = s.label;
      return c;
    }
    getSettings() {
      var s = trackState(this, "getSettings");
      var info = N.info(s.h);
      if (!info) return {};
      if (s.source !== "microphone") return { channelCount: info.channelCount, sampleRate: info.sampleRate };
      return {
        deviceId: info.deviceId,
        groupId: "",
        sampleRate: info.sampleRate,
        sampleSize: 16,
        channelCount: info.channelCount,
        latency: 0.01,
        // What runs on the capture side (see the header).
        echoCancellation: info.echoCancellation,
        noiseSuppression: info.noiseSuppression,
        autoGainControl: info.autoGainControl,
      };
    }
    getConstraints() {
      return shallow(trackState(this, "getConstraints").constraints);
    }
    getCapabilities() {
      var s = trackState(this, "getCapabilities");
      var info = N.info(s.h);
      if (!info || s.source !== "microphone") return {};
      return {
        deviceId: info.deviceId,
        groupId: "",
        sampleRate: { min: info.sampleRate, max: info.sampleRate },
        sampleSize: { min: 16, max: 16 },
        channelCount: { min: 1, max: info.channelCount },
        latency: { min: 0.01, max: 0.01 },
        echoCancellation: info.canProcess ? [true, false] : [false],
        noiseSuppression: info.canProcess ? [true, false] : [false],
        autoGainControl: info.canProcess ? [true, false] : [false],
      };
    }
    applyConstraints(constraints) {
      var self = this;
      return new Promise(function (resolve, reject) {
        var s = trackState(self, "applyConstraints");
        if (constraints !== undefined && (constraints === null || typeof constraints !== "object")) {
          throw new TypeErr("Failed to execute 'applyConstraints' on 'MediaStreamTrack': The provided value is not of type 'MediaTrackConstraints'.");
        }
        var want = readDevice(constraints && constraints.deviceId);
        var info = N.info(s.h);
        if (want.exact && info && want.id !== info.deviceId) {
          reject(new OverconstrainedError("deviceId", "Cannot switch a track to another device."));
          return;
        }
        N.advise("MediaStreamTrack.applyConstraints() records the constraints (getConstraints() returns them) but changes nothing about the capture.");
        s.constraints = shallow(constraints);
        resolve(undefined);
      });
    }
  }
  handlerAttr(MediaStreamTrack.prototype, "ended");
  handlerAttr(MediaStreamTrack.prototype, "mute");
  handlerAttr(MediaStreamTrack.prototype, "unmute");
  publish("MediaStreamTrack", MediaStreamTrack);

  function isTrack(t) {
    return T.has(t);
  }

  hooks.ended = function (nid) {
    var w = byNative.get(nid);
    var t = w && w.deref();
    byNative.delete(nid);
    if (!t) return;
    var s = T.get(t);
    if (s.ended) return;
    s.ended = true;
    t.dispatchEvent(new Ev("ended"));
  };

  // The native track id, for an embedder that reads a page's track (skathi
  // publishes it to a LiveKit room). 0 for anything else.
  method(g, "__rjs_mediaTrackId", function (t) {
    var s = T.get(t);
    return s ? s.nid : 0;
  });

  // ---------------------------------------------------------------------------
  // MediaStream
  // ---------------------------------------------------------------------------

  var M = new WeakMap(); // stream → { id, tracks }

  function streamState(ms, what) {
    var s = M.get(ms);
    if (!s) throw new TypeErr("Failed to execute '" + what + "' on 'MediaStream': Illegal invocation");
    return s;
  }

  function notATrack(what) {
    return new TypeErr("Failed to " + what + ": parameter 1 is not of type 'MediaStreamTrack'.");
  }

  class MediaStream extends ET {
    constructor(init) {
      super();
      var tracks = [];
      M.set(this, { id: uuid(), tracks: tracks });
      if (init === undefined) return;
      var from;
      if (M.has(init)) from = M.get(init).tracks;
      else if (init !== null && typeof init === "object" && typeof init[Symbol.iterator] === "function") from = Array.from(init);
      else throw new TypeErr("Failed to construct 'MediaStream': The provided value is not of type '(MediaStream or sequence<MediaStreamTrack>)'.");
      for (var i = 0; i < from.length; i++) {
        if (!isTrack(from[i])) throw new TypeErr("Failed to construct 'MediaStream': Failed to convert value to 'MediaStreamTrack'.");
        if (tracks.indexOf(from[i]) < 0) tracks.push(from[i]);
      }
    }
    get id() { return streamState(this, "id").id; }
    get active() {
      return streamState(this, "active").tracks.some(function (t) { return !T.get(t).ended; });
    }
    getTracks() { return streamState(this, "getTracks").tracks.slice(); }
    getAudioTracks() { return streamState(this, "getAudioTracks").tracks.slice(); }
    getVideoTracks() { return streamState(this, "getVideoTracks"), []; }
    getTrackById(id) {
      var ts = streamState(this, "getTrackById").tracks;
      id = String(id);
      for (var i = 0; i < ts.length; i++) if (T.get(ts[i]).id === id) return ts[i];
      return null;
    }
    addTrack(track) {
      var ts = streamState(this, "addTrack").tracks;
      if (!isTrack(track)) throw notATrack("execute 'addTrack' on 'MediaStream'");
      if (ts.indexOf(track) < 0) ts.push(track);
    }
    removeTrack(track) {
      var ts = streamState(this, "removeTrack").tracks;
      if (!isTrack(track)) throw notATrack("execute 'removeTrack' on 'MediaStream'");
      var i = ts.indexOf(track);
      if (i >= 0) ts.splice(i, 1);
    }
    clone() {
      var ts = streamState(this, "clone").tracks;
      return new MediaStream(ts.map(function (t) { return t.clone(); }));
    }
  }
  handlerAttr(MediaStream.prototype, "addtrack");
  handlerAttr(MediaStream.prototype, "removetrack");
  publish("MediaStream", MediaStream);

  // ---------------------------------------------------------------------------
  // getUserMedia
  // ---------------------------------------------------------------------------

  // `deviceId` as a constraint: a bare value is "ideal" (fall back to the
  // default), `{ exact }` is required (a missing device is Overconstrained).
  function readDevice(c) {
    var exact = false;
    if (c && typeof c === "object" && !Array.isArray(c)) {
      if (c.exact !== undefined) { exact = true; c = c.exact; }
      else c = c.ideal;
    }
    if (Array.isArray(c)) c = c[0];
    return { id: typeof c === "string" && c !== "" ? c : null, exact: exact && typeof c === "string" };
  }

  function asked(v) {
    if (v === true) return true;
    if (v && typeof v === "object") return v.exact === true || v.ideal === true;
    return false;
  }

  // Chrome's default for the three processing switches: on, unless the
  // constraint says `false` (bare, `exact` or `ideal`).
  function wanted(v) {
    if (typeof v === "boolean") return v;
    if (v && typeof v === "object") {
      if (typeof v.exact === "boolean") return v.exact;
      if (typeof v.ideal === "boolean") return v.ideal;
    }
    return true;
  }

  var pending = new Map();
  var nextToken = 1;

  var MD = g.MediaDevices;
  if (typeof MD === "function") {
    method(MD.prototype, "getUserMedia", function getUserMedia(constraints) {
      var self = this;
      return new Promise(function (resolve, reject) {
        if (!(self instanceof MD)) throw new TypeErr("Illegal invocation");
        if (constraints !== undefined && constraints !== null && typeof constraints !== "object") {
          throw new TypeErr("Failed to execute 'getUserMedia' on 'MediaDevices': The provided value is not of type 'MediaStreamConstraints'.");
        }
        var c = constraints || {};
        if (!c.audio && !c.video) {
          throw new TypeErr("Failed to execute 'getUserMedia' on 'MediaDevices': At least one of audio and video must be requested");
        }
        if (c.video) {
          N.advise("getUserMedia({ video }) is refused: this runtime captures audio only.");
          reject(named("NotSupportedError", "Video capture is not supported by this runtime."));
          return;
        }
        var ac = c.audio === true ? {} : c.audio;
        if (
          !N.canProcess() &&
          (asked(ac.echoCancellation) || asked(ac.noiseSuppression) || asked(ac.autoGainControl))
        ) {
          N.advise(
            "getUserMedia: echoCancellation / noiseSuppression / autoGainControl were requested, but this " +
              "host has no capture processing, so the track is the raw device signal (getSettings() reports false)."
          );
        }
        var dev = readDevice(ac.deviceId);
        var token = nextToken++;
        var refused = N.open(
          token,
          dev.id,
          dev.exact,
          wanted(ac.echoCancellation),
          wanted(ac.noiseSuppression),
          wanted(ac.autoGainControl)
        );
        if (refused) {
          reject(failure(refused[0], refused[1]));
          return;
        }
        pending.set(token, { resolve: resolve, reject: reject, constraints: ac });
      });
    });

    if (typeof MD.prototype.getSupportedConstraints !== "function") {
      method(MD.prototype, "getSupportedConstraints", function getSupportedConstraints() {
        return {
          deviceId: true,
          groupId: true,
          sampleRate: true,
          sampleSize: true,
          channelCount: true,
          latency: true,
          echoCancellation: true,
          noiseSuppression: true,
          autoGainControl: true,
        };
      });
    }
  }

  // A track an embedder hands the page (mediastream.rs `js_track`): a remote
  // call participant's audio, say.
  hooks.wrap = function (handle) {
    return new MediaStreamTrack(NEW, handle, {});
  };

  hooks.settle = function (token, handle, name, message) {
    var p = pending.get(token);
    if (!p) return;
    pending.delete(token);
    if (handle) p.resolve(new MediaStream([new MediaStreamTrack(NEW, handle, p.constraints)]));
    else p.reject(failure(name, message));
  };

  // ---------------------------------------------------------------------------
  // The Web Audio nodes
  // ---------------------------------------------------------------------------

  var AC = g.AudioContext;
  var AN = g.AudioNode;
  if (typeof AC === "function" && typeof AN === "function") {
    var sourceStream = new WeakMap();
    var destStream = new WeakMap();

    function needContext(ctx, iface) {
      if (!(ctx instanceof AC)) {
        throw new TypeErr("Failed to construct '" + iface + "': parameter 1 is not of type 'BaseAudioContext'.");
      }
    }

    // A node object is native (it connects like every other node), so the
    // constructors return it instead of the `this` a JS class would make.
    class MediaStreamAudioSourceNode extends AN {
      constructor(context, options) {
        needContext(context, "MediaStreamAudioSourceNode");
        if (!options || typeof options !== "object" || options.mediaStream === undefined) {
          throw new TypeErr("Failed to construct 'MediaStreamAudioSourceNode': Failed to read the 'mediaStream' property from 'MediaStreamAudioSourceOptions': Required member is undefined.");
        }
        var ms = options.mediaStream;
        if (!M.has(ms)) {
          throw new TypeErr("Failed to construct 'MediaStreamAudioSourceNode': Failed to read the 'mediaStream' property from 'MediaStreamAudioSourceOptions': Failed to convert value to 'MediaStream'.");
        }
        // Web Audio §1.24.1: the audio track whose id sorts first, fixed now.
        var audio = M.get(ms).tracks.slice().sort(function (a, b) {
          var x = T.get(a).id, y = T.get(b).id;
          return x < y ? -1 : x > y ? 1 : 0;
        });
        if (!audio.length) {
          throw named("InvalidStateError", "Failed to construct 'MediaStreamAudioSourceNode': MediaStream has no audio track");
        }
        var node = N.sourceNode(context, T.get(audio[0]).h, new.target.prototype);
        sourceStream.set(node, ms);
        return node;
      }
      get mediaStream() {
        if (!sourceStream.has(this)) throw new TypeErr("Illegal invocation");
        return sourceStream.get(this);
      }
    }

    class MediaStreamAudioDestinationNode extends AN {
      constructor(context, options) {
        needContext(context, "MediaStreamAudioDestinationNode");
        var cc = options && options.channelCount !== undefined ? Number(options.channelCount) : 2;
        if (!(cc >= 1 && cc <= 32)) {
          throw named("NotSupportedError", "Failed to construct 'MediaStreamAudioDestinationNode': The channel count provided (" + cc + ") is outside the range [1, 32].");
        }
        if (cc > 2) N.advise("MediaStreamAudioDestinationNode: channelCount " + cc + " is mixed to stereo; the track carries 2 channels.");
        var pair = N.destNode(context, new.target.prototype, cc);
        var ms = new MediaStream([new MediaStreamTrack(NEW, pair[1], {})]);
        destStream.set(pair[0], ms);
        return pair[0];
      }
      get stream() {
        if (!destStream.has(this)) throw new TypeErr("Illegal invocation");
        return destStream.get(this);
      }
    }

    publish("MediaStreamAudioSourceNode", MediaStreamAudioSourceNode);
    publish("MediaStreamAudioDestinationNode", MediaStreamAudioDestinationNode);

    method(AC.prototype, "createMediaStreamSource", function createMediaStreamSource(mediaStream) {
      return new MediaStreamAudioSourceNode(this, { mediaStream: mediaStream });
    });
    method(AC.prototype, "createMediaStreamDestination", function createMediaStreamDestination() {
      return new MediaStreamAudioDestinationNode(this);
    });
  }

  return hooks;
});
