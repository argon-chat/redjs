// `WebSocketStream` — the stream-shaped WebSocket API (WHATWG WebSockets
// Standard, "WebSocketStream"; shipped in Chromium since 124).
//
// WHY THIS EXISTS HERE, ON TOP OF THE NATIVE `WebSocket`
//
// It is not a nicety. The concrete consumer this whole module was written for
// — `@argon-chat/ion.webcore`, the RPC layer the Argon meet app's entire
// control plane runs on — opens EVERY streaming call with
//
//     if (typeof WebSocketStream > "u")
//         throw new Error("WebSocketStream is not supported in this browser");
//     const s = new WebSocketStream(url, { signal, protocols: [...] });
//     const { readable, writable } = await s.opened;
//
// so a runtime with `WebSocket` and without `WebSocketStream` fails that check
// and no RPC is possible at all. Shipping the classic API alone would have
// been a milestone that looks complete and unblocks nothing.
//
// It is JS rather than Rust because it is genuinely a *composition*: every
// byte still goes through the native `WebSocket` above, and the queueing,
// backpressure and cancellation are the native `ReadableStream` /
// `WritableStream` cores. There is no I/O here to move off-thread.
//
// TWO HONEST NARROWINGS, both about backpressure:
//
//  * A browser stops READING from the socket when the readable stream's queue
//    is full. Our reader thread always drains the socket, so a consumer that
//    stops reading grows a queue in memory instead of pushing back on TCP.
//  * A write's promise resolves as soon as the bytes are queued, unless
//    `bufferedAmount` is already over HIGH_WATER — then it waits for the
//    socket to drain below it. That is real backpressure driven by the real
//    `bufferedAmount`, but it is coarser than a browser's per-chunk accounting.
(function (g) {
  "use strict";
  if (typeof g.WebSocket !== "function" || typeof g.ReadableStream !== "function") return;
  if (typeof g.WebSocketStream === "function") return;

  // Bytes allowed to sit in the socket's send queue before `writer.write()`
  // stops resolving immediately. Small RPC frames never touch this path.
  var HIGH_WATER = 1 << 20;
  var DRAIN_POLL_MS = 4;

  // The spec's "a new `name` DOMException" — the real class, installed by redjs
  // before this script runs, captured so a page that reassigns the global
  // cannot change what this throws.
  var DOMEx = g.DOMException;
  function named(name, message) {
    return new DOMEx(message, name);
  }

  // Marks a promise handled so an unobserved `closed` rejection is not noise;
  // a `.catch()` the caller attaches later still receives it.
  function quiet(p) {
    p.catch(function () {});
    return p;
  }

  function drained(ws) {
    if (ws.bufferedAmount <= HIGH_WATER) return undefined;
    return new Promise(function (resolve) {
      var tick = function () {
        if (ws.readyState !== WebSocket.OPEN || ws.bufferedAmount <= HIGH_WATER) resolve();
        else setTimeout(tick, DRAIN_POLL_MS);
      };
      setTimeout(tick, DRAIN_POLL_MS);
    });
  }

  class WebSocketStream {
    constructor(url, options) {
      options = options || {};
      var protocols = options.protocols === undefined ? [] : options.protocols;
      var signal = options.signal;

      // Constructing the socket is what validates the URL and the protocol
      // list, so a bad argument throws the same named error it would from
      // `new WebSocket(...)` — synchronously, as in Chromium.
      var ws = new WebSocket(url, protocols);
      // The readable yields ArrayBuffers for binary frames; that is what
      // Chromium delivers and what consumers brand-check against.
      ws.binaryType = "arraybuffer";
      this._ws = ws;
      this._url = ws.url;

      var openedResolve, openedReject, closedResolve, closedReject;
      var settledOpen = false;
      this.opened = new Promise(function (res, rej) {
        openedResolve = res;
        openedReject = rej;
      });
      this.closed = quiet(
        new Promise(function (res, rej) {
          closedResolve = res;
          closedReject = rej;
        })
      );

      var controller = null;
      var readable = new ReadableStream({
        start: function (c) {
          controller = c;
        },
        // Cancelling the read side closes the socket, per the spec's
        // "cancel the WebSocketStream".
        cancel: function () {
          try {
            ws.close();
          } catch (e) {}
        },
      });

      var writable = new WritableStream({
        write: function (chunk) {
          if (ws.readyState !== WebSocket.OPEN) {
            throw named("InvalidStateError", "WebSocketStream: the socket is not open");
          }
          ws.send(chunk);
          return drained(ws);
        },
        close: function () {
          try {
            ws.close();
          } catch (e) {}
        },
        abort: function () {
          try {
            ws.close();
          } catch (e) {}
        },
      });

      ws.onopen = function () {
        settledOpen = true;
        openedResolve({
          readable: readable,
          writable: writable,
          protocol: ws.protocol,
          extensions: ws.extensions,
        });
      };

      ws.onmessage = function (e) {
        if (controller) controller.enqueue(e.data);
      };

      ws.onclose = function (e) {
        var info = { closeCode: e.code, reason: e.reason };
        if (!settledOpen) {
          settledOpen = true;
          // The connection never came up: `opened` is the promise that has to
          // say so, and it must not resolve with half a stream pair.
          openedReject(named("NetworkError", "WebSocketStream: the connection could not be established"));
        }
        if (e.wasClean) {
          if (controller) {
            try {
              controller.close();
            } catch (err) {}
          }
          closedResolve(info);
        } else {
          var err = named("NetworkError", "WebSocketStream: the connection closed abnormally (" + e.code + ")");
          if (controller) {
            try {
              controller.error(err);
            } catch (e2) {}
          }
          closedReject(err);
        }
      };

      // `error` needs no handler of its own: it is always followed by `close`,
      // which is where both promises settle. Registering one only keeps the
      // event from looking unhandled.
      ws.onerror = function () {};

      if (signal) {
        if (signal.aborted) {
          try {
            ws.close();
          } catch (e) {}
        } else {
          signal.addEventListener(
            "abort",
            function () {
              try {
                ws.close();
              } catch (e) {}
            },
            { once: true }
          );
        }
      }
    }

    get url() {
      return this._url;
    }

    close(closeInfo) {
      closeInfo = closeInfo || {};
      var reason = closeInfo.reason === undefined ? "" : closeInfo.reason;
      try {
        if (closeInfo.closeCode === undefined) this._ws.close(undefined, reason);
        else this._ws.close(closeInfo.closeCode, reason);
      } catch (e) {
        // A double close, or a close after the socket is gone, is a no-op —
        // and the consumer above calls `close()` from several catch paths.
        if (e && (e.name === "InvalidAccessError" || e.name === "SyntaxError")) throw e;
      }
    }
  }

  Object.defineProperty(WebSocketStream.prototype, Symbol.toStringTag, {
    value: "WebSocketStream",
    configurable: true,
  });
  Object.defineProperty(g, "WebSocketStream", {
    value: WebSocketStream,
    writable: true,
    enumerable: false,
    configurable: true,
  });
})(globalThis);
