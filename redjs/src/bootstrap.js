// Fallback for embedders built without the native canvas feature.
(function (g) {
  'use strict';

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
