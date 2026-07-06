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

})(globalThis);
