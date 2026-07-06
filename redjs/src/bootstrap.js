// redjs bootstrap — ONLY the dumb stubs. Everything real (EventTarget/Event,
// TextEncoder/Decoder, Blob, URL/URLSearchParams, fetch/Response/Headers,
// Image/HTMLImageElement/ImageBitmap/createImageBitmap, document/window,
// timers/rAF/performance) is defined NATIVELY by redjs (src/webapi, src/web).
(function (g) {
  'use strict';

  // ── console: prefer a host-registered `__hostConsole` (editor pipe);
  //    otherwise the native __rjs_print. Real JS functions either way. ──
  (function () {
    var host = (typeof __hostConsole !== 'undefined') ? __hostConsole : null;
    function fmt(args) {
      var parts = [];
      for (var i = 0; i < args.length; i++) {
        var a = args[i];
        if (typeof a === 'string') { parts.push(a); continue; }
        if (a instanceof Error) { parts.push(a.stack || a.message); continue; }
        try { parts.push(JSON.stringify(a)); } catch (e) { parts.push(String(a)); }
      }
      return parts.join(' ');
    }
    var forward = host
      ? function (name) { return function (...args) { try { host[name](...args); } catch (e) {} }; }
      : function (name) {
          var level = name === 'error' ? 3 : name === 'warn' ? 2 : 1;
          return function (...args) { try { __rjs_print(level, fmt(args)); } catch (e) {} };
        };
    var noop = function () {};
    g.console = {
      log: forward('log'), info: forward('info'), warn: forward('warn'),
      error: forward('error'), debug: forward('debug'), trace: forward('trace'),
      clear: forward('clear'),
      assert: function (cond, ...args) { if (!cond) g.console.error('Assertion failed:', ...args); },
      dir: forward('log'), table: forward('log'),
      group: forward('log'), groupCollapsed: forward('log'), groupEnd: noop,
      count: noop, countReset: noop, time: noop, timeEnd: noop, timeLog: noop,
    };
  })();

  // ── microtask / immediate ──
  if (typeof g.queueMicrotask !== 'function') {
    g.queueMicrotask = function (cb) { Promise.resolve().then(cb); };
  }
  if (typeof g.setImmediate !== 'function') {
    g.setImmediate = function (cb) {
      var a = Array.prototype.slice.call(arguments, 1);
      return setTimeout(function () { cb.apply(null, a); }, 0);
    };
    g.clearImmediate = function (id) { clearTimeout(id); };
  }

  // ── atob / btoa (latin1 string ↔ base64) ──
  if (typeof g.atob !== 'function') {
    var B64 = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';
    g.atob = function (input) {
      var s = String(input).replace(/[\t\n\f\r ]+/g, '');
      if (/[^A-Za-z0-9+/=]/.test(s)) throw new Error('atob: invalid character');
      s = s.replace(/=+$/, '');
      var out = '', buffer = 0, bits = 0;
      for (var i = 0; i < s.length; i++) {
        buffer = (buffer << 6) | B64.indexOf(s[i]);
        bits += 6;
        if (bits >= 8) { bits -= 8; out += String.fromCharCode((buffer >> bits) & 0xff); }
      }
      return out;
    };
    g.btoa = function (input) {
      var s = String(input), out = '';
      for (var i = 0; i < s.length; i += 3) {
        var c0 = s.charCodeAt(i), c1 = s.charCodeAt(i + 1), c2 = s.charCodeAt(i + 2);
        if (c0 > 255 || c1 > 255 || c2 > 255) throw new Error('btoa: character out of latin1 range');
        var triplet = (c0 << 16) | ((c1 || 0) << 8) | (c2 || 0);
        out += B64[(triplet >> 18) & 63] + B64[(triplet >> 12) & 63];
        out += i + 1 < s.length ? B64[(triplet >> 6) & 63] : '=';
        out += i + 2 < s.length ? B64[triplet & 63] : '=';
      }
      return out;
    };
  }

  // ── AbortController / AbortSignal (no async cancellation sources here) ──
  if (typeof g.AbortController !== 'function') {
    g.AbortSignal = class AbortSignal {
      constructor() { this.aborted = false; this.reason = undefined; this.onabort = null; }
      addEventListener() {} removeEventListener() {} throwIfAborted() { if (this.aborted) throw new Error('Aborted'); }
    };
    g.AbortController = class AbortController {
      constructor() { this.signal = new g.AbortSignal(); }
      abort(reason) { this.signal.aborted = true; this.signal.reason = reason; if (typeof this.signal.onabort === 'function') this.signal.onabort(); }
    };
  }

  // ── Request (minimal record) ──
  if (typeof g.Request !== 'function') {
    g.Request = class Request {
      constructor(input, init) { init = init || {}; this.url = String((input && input.url) || input || ''); this.method = init.method || 'GET'; this.headers = new g.Headers(); }
    };
  }

  // ── ImageData (plain pixel record; consumed by the native image pipeline) ──
  if (typeof g.ImageData !== 'function') {
    g.ImageData = class ImageData {
      constructor(dataOrW, wOrH, h) {
        if (typeof dataOrW === 'number') {
          this.width = dataOrW | 0; this.height = wOrH | 0;
          this.data = new Uint8ClampedArray(this.width * this.height * 4);
        } else {
          this.data = dataOrW;
          this.width = wOrH | 0;
          this.height = (h != null ? h : (dataOrW.length / 4 / wOrH)) | 0;
        }
        this.colorSpace = 'srgb';
      }
      get __brand() { return 'ImageData'; }
    };
  }

  // ── instanceof target for the HOST canvas wrapper (C# object, __brand) ──
  (function () {
    var fn = function () {};
    Object.defineProperty(fn, Symbol.hasInstance, {
      value: function (obj) {
        if (obj == null) return false;
        try { return obj.__brand === 'HTMLCanvasElement'; } catch (e) { return false; }
      },
    });
    g.HTMLCanvasElement = fn;
  })();
  if (typeof g.HTMLVideoElement !== 'function') {
    g.HTMLVideoElement = class HTMLVideoElement {};
  }

  // ── BroadcastChannel stub ──
  if (typeof g.BroadcastChannel !== 'function') {
    g.BroadcastChannel = class BroadcastChannel { constructor(name) { this.name = name; this.onmessage = null; } postMessage() {} close() {} addEventListener() {} removeEventListener() {} };
  }

  // ── location ──
  if (typeof g.location === 'undefined') {
    g.location = { href: 'app://redpew/', origin: 'app://redpew', protocol: 'app:', host: 'redpew', pathname: '/', search: '', hash: '', reload: function () {} };
  }

  // ── OffscreenCanvas + 2D context (drawImage/getImageData over native pixels) ──
  if (typeof g.OffscreenCanvas !== 'function') {
    class Ctx2D {
      constructor(w, h) {
        this.canvas = null; this.__w = w | 0; this.__h = h | 0;
        this.__px = new Uint8ClampedArray(Math.max(0, this.__w * this.__h * 4));
        this.fillStyle = '#000'; this.strokeStyle = '#000'; this.globalAlpha = 1; this.imageSmoothingEnabled = true;
      }
      drawImage(src, dx, dy) {
        dx = (dx || 0) | 0; dy = (dy || 0) | 0;
        var u = __rjs_sourcePixels(src); if (!u) return;
        var sw = src.width | 0, sh = src.height | 0, W = this.__w, H = this.__h, px = this.__px;
        for (var y = 0; y < sh; y++) {
          var ty = dy + y; if (ty < 0 || ty >= H) continue;
          for (var x = 0; x < sw; x++) {
            var tx = dx + x; if (tx < 0 || tx >= W) continue;
            var si = (y * sw + x) * 4, ti = (ty * W + tx) * 4;
            px[ti] = u[si]; px[ti + 1] = u[si + 1]; px[ti + 2] = u[si + 2]; px[ti + 3] = u[si + 3];
          }
        }
      }
      getImageData(sx, sy, sw, sh) {
        sx = sx | 0; sy = sy | 0; sw = sw | 0; sh = sh | 0;
        var out = new Uint8ClampedArray(sw * sh * 4), W = this.__w, H = this.__h, px = this.__px;
        for (var y = 0; y < sh; y++) {
          var ry = sy + y; if (ry < 0 || ry >= H) continue;
          for (var x = 0; x < sw; x++) {
            var rx = sx + x; if (rx < 0 || rx >= W) continue;
            var si = (ry * W + rx) * 4, di = (y * sw + x) * 4;
            out[di] = px[si]; out[di + 1] = px[si + 1]; out[di + 2] = px[si + 2]; out[di + 3] = px[si + 3];
          }
        }
        return new g.ImageData(out, sw, sh);
      }
      createImageData(w, h) { return new g.ImageData(w | 0, h | 0); }
      putImageData() {} clearRect() {} fillRect() {} strokeRect() {} save() {} restore() {}
      translate() {} scale() {} rotate() {} setTransform() {} resetTransform() {}
      beginPath() {} closePath() {} moveTo() {} lineTo() {} arc() {} rect() {} fill() {} stroke() {} clip() {}
    }
    g.OffscreenCanvasRenderingContext2D = Ctx2D;
    g.OffscreenCanvas = class OffscreenCanvas {
      constructor(w, h) { this.width = w | 0; this.height = h | 0; }
      getContext(type) { return (type === '2d') ? new Ctx2D(this.width, this.height) : null; }
      transferToImageBitmap() { return null; }
    };
  }

  // ── Web Audio API (SILENT stub — full node-graph surface, no output) ──
  if (typeof g.AudioContext !== 'function') {
    class AudioParam {
      constructor(v) { this.value = (v == null ? 0 : v); this.defaultValue = this.value; this.minValue = -3.4028235e38; this.maxValue = 3.4028235e38; this.automationRate = 'a-rate'; }
      setValueAtTime(v) { this.value = v; return this; }
      setTargetAtTime(v) { this.value = v; return this; }
      linearRampToValueAtTime(v) { this.value = v; return this; }
      exponentialRampToValueAtTime(v) { this.value = v; return this; }
      setValueCurveAtTime() { return this; }
      cancelScheduledValues() { return this; }
      cancelAndHoldAtTime() { return this; }
    }
    class AudioNode {
      constructor(ctx) { this.context = ctx; this.numberOfInputs = 1; this.numberOfOutputs = 1; this.channelCount = 2; this.channelCountMode = 'max'; this.channelInterpretation = 'speakers'; }
      connect(dst) { return dst; } disconnect() {}
    }
    class GainNode extends AudioNode { constructor(ctx) { super(ctx); this.gain = new AudioParam(1); } }
    class StereoPannerNode extends AudioNode { constructor(ctx) { super(ctx); this.pan = new AudioParam(0); } }
    class BiquadFilterNode extends AudioNode {
      constructor(ctx) { super(ctx); this.type = 'lowpass'; this.frequency = new AudioParam(350); this.Q = new AudioParam(1); this.gain = new AudioParam(0); this.detune = new AudioParam(0); }
      getFrequencyResponse() {}
    }
    class AnalyserNode extends AudioNode {
      constructor(ctx) { super(ctx); this.fftSize = 2048; this.frequencyBinCount = 1024; this.minDecibels = -100; this.maxDecibels = -30; this.smoothingTimeConstant = 0.8; }
      getByteFrequencyData(a) { if (a && a.fill) a.fill(0); }
      getByteTimeDomainData(a) { if (a && a.fill) a.fill(128); }
      getFloatFrequencyData(a) { if (a && a.fill) a.fill(-Infinity); }
      getFloatTimeDomainData(a) { if (a && a.fill) a.fill(0); }
    }
    class PannerNode extends AudioNode {
      constructor(ctx) {
        super(ctx);
        this.positionX = new AudioParam(0); this.positionY = new AudioParam(0); this.positionZ = new AudioParam(0);
        this.orientationX = new AudioParam(1); this.orientationY = new AudioParam(0); this.orientationZ = new AudioParam(0);
        this.panningModel = 'equalpower'; this.distanceModel = 'inverse'; this.refDistance = 1; this.maxDistance = 10000;
        this.rolloffFactor = 1; this.coneInnerAngle = 360; this.coneOuterAngle = 360; this.coneOuterGain = 0;
      }
      setPosition(x, y, z) { this.positionX.value = x; this.positionY.value = y; this.positionZ.value = z; }
      setOrientation(x, y, z) { this.orientationX.value = x; this.orientationY.value = y; this.orientationZ.value = z; }
    }
    class AudioBufferSourceNode extends AudioNode {
      constructor(ctx) { super(ctx); this.buffer = null; this.loop = false; this.loopStart = 0; this.loopEnd = 0; this.playbackRate = new AudioParam(1); this.detune = new AudioParam(0); this.onended = null; this.__started = false; }
      start() { this.__started = true; }
      stop() {
        if (typeof this.onended === 'function') {
          var cb = this.onended; this.onended = null;
          queueMicrotask(function () { try { cb(); } catch (e) { console.error(e); } });
        }
      }
    }
    class AudioBuffer {
      constructor(opts) {
        opts = opts || {};
        this.numberOfChannels = opts.numberOfChannels || 2;
        this.length = opts.length || 0;
        this.sampleRate = opts.sampleRate || 44100;
        this.duration = this.length / this.sampleRate;
        this.__ch = [];
        for (var i = 0; i < this.numberOfChannels; i++) this.__ch.push(new Float32Array(this.length));
      }
      getChannelData(c) { return this.__ch[c] || (this.__ch[c] = new Float32Array(this.length)); }
      copyFromChannel(dst, c) { var s = this.getChannelData(c); if (dst && dst.set) dst.set(s.subarray(0, dst.length)); }
      copyToChannel(src, c) { var d = this.getChannelData(c); if (src && d) d.set(src.subarray ? src.subarray(0, d.length) : src); }
    }
    class AudioListener {
      constructor() {
        this.positionX = new AudioParam(0); this.positionY = new AudioParam(0); this.positionZ = new AudioParam(0);
        this.forwardX = new AudioParam(0); this.forwardY = new AudioParam(0); this.forwardZ = new AudioParam(-1);
        this.upX = new AudioParam(0); this.upY = new AudioParam(1); this.upZ = new AudioParam(0);
      }
      setPosition(x, y, z) { this.positionX.value = x; this.positionY.value = y; this.positionZ.value = z; }
      setOrientation(fx, fy, fz, ux, uy, uz) { this.forwardX.value = fx; this.forwardY.value = fy; this.forwardZ.value = fz; this.upX.value = ux; this.upY.value = uy; this.upZ.value = uz; }
    }
    class BaseAudioContext {
      constructor(opts) {
        opts = opts || {};
        this.sampleRate = opts.sampleRate || 44100; this.state = 'running'; this.baseLatency = 0; this.outputLatency = 0;
        this.listener = new AudioListener();
        this.destination = Object.assign(new AudioNode(this), { channelCount: 2, maxChannelCount: 2 });
        this.audioWorklet = { addModule: function () { return Promise.resolve(); } };
        this.__t0 = (typeof performance !== 'undefined' ? performance.now() : 0);
        this.onstatechange = null;
      }
      get currentTime() { return ((typeof performance !== 'undefined' ? performance.now() : 0) - this.__t0) / 1000; }
      createGain() { return new GainNode(this); }
      createAnalyser() { return new AnalyserNode(this); }
      createBufferSource() { return new AudioBufferSourceNode(this); }
      createPanner() { return new PannerNode(this); }
      createStereoPanner() { return new StereoPannerNode(this); }
      createBiquadFilter() { return new BiquadFilterNode(this); }
      createOscillator() { var n = new AudioBufferSourceNode(this); n.frequency = new AudioParam(440); n.detune = new AudioParam(0); n.type = 'sine'; return n; }
      createConvolver() { var n = new AudioNode(this); n.buffer = null; n.normalize = true; return n; }
      createDelay() { var n = new AudioNode(this); n.delayTime = new AudioParam(0); return n; }
      createDynamicsCompressor() { var n = new AudioNode(this); n.threshold = new AudioParam(-24); n.knee = new AudioParam(30); n.ratio = new AudioParam(12); n.attack = new AudioParam(0.003); n.release = new AudioParam(0.25); n.reduction = 0; return n; }
      createWaveShaper() { var n = new AudioNode(this); n.curve = null; n.oversample = 'none'; return n; }
      createBuffer(numberOfChannels, length, sampleRate) { return new AudioBuffer({ numberOfChannels: numberOfChannels, length: length, sampleRate: sampleRate }); }
      createMediaElementSource() { return new AudioNode(this); }
      createMediaStreamSource() { return new AudioNode(this); }
      decodeAudioData(data, success, error) {
        try {
          var byteLen = data ? (data.byteLength || data.length || 0) : 0;
          var seconds = Math.min(600, Math.max(0.05, byteLen / 16000));
          var buf = new AudioBuffer({ numberOfChannels: 2, length: Math.floor(seconds * this.sampleRate), sampleRate: this.sampleRate });
          if (typeof success === 'function') success(buf);
          return Promise.resolve(buf);
        } catch (e) {
          if (typeof error === 'function') error(e);
          return Promise.reject(e);
        }
      }
      resume() { this.state = 'running'; return Promise.resolve(); }
      suspend() { this.state = 'suspended'; return Promise.resolve(); }
      close() { this.state = 'closed'; return Promise.resolve(); }
    }
    g.AudioParam = AudioParam; g.AudioNode = AudioNode; g.GainNode = GainNode; g.AnalyserNode = AnalyserNode;
    g.BiquadFilterNode = BiquadFilterNode; g.PannerNode = PannerNode; g.StereoPannerNode = StereoPannerNode;
    g.AudioBufferSourceNode = AudioBufferSourceNode; g.AudioBuffer = AudioBuffer; g.AudioListener = AudioListener;
    g.AudioDestinationNode = AudioNode;
    g.AudioContext = class AudioContext extends BaseAudioContext {};
    g.webkitAudioContext = g.AudioContext;
    g.OfflineAudioContext = class OfflineAudioContext extends BaseAudioContext {
      constructor(a, b, c) { super({ sampleRate: (typeof a === 'object' ? a.sampleRate : c) }); this.length = (typeof a === 'object' ? a.length : b); }
      startRendering() { return Promise.resolve(new AudioBuffer({ numberOfChannels: 2, length: this.length || 0, sampleRate: this.sampleRate })); }
    };
  }
})(globalThis);
