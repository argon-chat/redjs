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
    function isError(a) {
      if (!a || typeof a !== 'object') return false;
      if (a instanceof Error) return true;
      var tag = Object.prototype.toString.call(a);
      return tag === '[object Error]' || tag === '[object DOMException]';
    }
    function errorHead(a) {
      return (a.name || 'Error') + (a.message ? ': ' + a.message : '');
    }
    // One argument, as a console prints a value that is not part of a format string.
    function show(a) {
      if (typeof a === 'string') return a;
      // `stack` FIRST would drop the message, and that is not a style preference — it is a
      // V8 assumption that does not hold here. V8's `Error.prototype.stack` begins with a
      // "Name: message" header line; **JSC's does not** — it is frames only. So `a.stack`
      // alone silently discards the single most useful part of every error ever logged in
      // this runtime, and the fallback to `a.message` never runs because `stack` is truthy.
      //
      // Emit the header ourselves, then the frames.
      if (isError(a)) {
        var head = errorHead(a);
        return a.stack ? head + '\n' + a.stack : head;
      }
      // Inside an object an Error is its header, not JSON's `{}` (its fields are
      // non-enumerable), and a BigInt prints as `1n` instead of throwing.
      try {
        var s = JSON.stringify(a, function (_k, x) {
          if (typeof x === 'bigint') return x + 'n';
          return isError(x) ? errorHead(x) : x;
        });
        return s === undefined ? String(a) : s;
      } catch (e) { return String(a); }
    }
    // The WHATWG Console Standard's Formatter (§2.2), for a first argument that is a
    // format string: %s %d %i %f %o %O %j, %% for a literal percent, and %c, which
    // consumes its argument as CSS and prints nothing. Without it every
    // `console.log("%cinfo%c %s", css, "", msg)` (consola, the Argon client's logger)
    // reached stderr as the literal `%c` plus the multi-line CSS. Leftover arguments are
    // appended space-separated; a specifier with no argument left stays as written.
    var SPEC = /%([sdifoOjc%])/g;
    function fmt(args) {
      if (typeof args[0] !== 'string' || args.length < 2 || !/%[sdifoOjc%]/.test(args[0])) {
        return args.map(show).join(' ');
      }
      var next = 1;
      var out = args[0].replace(SPEC, function (whole, kind) {
        if (kind === '%') return '%';
        if (next >= args.length) return whole;
        var a = args[next++];
        switch (kind) {
          case 'c': return '';
          case 'd':
          case 'i': return typeof a === 'symbol' || typeof a === 'bigint' ? String(a) : String(parseInt(a, 10));
          case 'f': return typeof a === 'symbol' ? String(a) : String(parseFloat(a));
          case 's': return typeof a === 'string' ? a : show(a);
          default: return show(a);
        }
      });
      for (; next < args.length; next++) out += ' ' + show(args[next]);
      return out;
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

  // ── Request (minimal record) ──
  if (typeof g.Request !== 'function') {
    g.Request = class Request {
      constructor(input, init) { init = init || {}; this.url = String((input && input.url) || input || ''); this.method = init.method || 'GET'; this.headers = new g.Headers(); }
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
    Object.defineProperty(fn.prototype, Symbol.toStringTag, { value: 'HTMLCanvasElement', configurable: true });
    g.HTMLCanvasElement = fn;
  })();
  if (typeof g.HTMLVideoElement !== 'function') {
    g.HTMLVideoElement = class HTMLVideoElement {};
    Object.defineProperty(g.HTMLVideoElement.prototype, Symbol.toStringTag, { value: 'HTMLVideoElement', configurable: true });
  }

  // ── location ──
  if (typeof g.location === 'undefined') {
    g.location = { href: 'app://redjs/', origin: 'app://redjs', protocol: 'app:', host: 'redjs', pathname: '/', search: '', hash: '', reload: function () {} };
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
    Object.defineProperty(Ctx2D.prototype, Symbol.toStringTag, { value: 'OffscreenCanvasRenderingContext2D', configurable: true });
    Object.defineProperty(g.OffscreenCanvas.prototype, Symbol.toStringTag, { value: 'OffscreenCanvas', configurable: true });
  }

})(globalThis);
