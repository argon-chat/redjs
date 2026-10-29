// WHATWG Streams — the promise-shaped surface.
//
// This file is deliberately thin and deliberately JavaScript. Every DECISION it
// makes is delegated to `RedReadableCore` / `RedWritableCore` (Rust,
// src/webapi/streams.rs): the queue and its running total, desiredSize, whether
// to pull, whether a close may land yet, and whether backpressure changed. What
// is left here is promise sequencing over USER CALLBACKS — `start`, `pull`,
// `write`, `transform`, `size` — which cannot leave JS, because a promise and a
// chunk are both JS values.
//
// Chunk VALUES never cross the boundary at all: they sit in `_values` beside
// the Rust queue, kept in lockstep by index. So there is no marshalling per
// chunk and no GC protect/unprotect per chunk either.
//
// Deliberate narrowings, each with its spec reference:
//
//  * BYTE STREAMS. `new ReadableStream(src, …)` with `src.type === 'bytes'`
//    (Streams §4.2.4, the underlying byte source) and BYOB readers
//    (`getReader({ mode: 'byob' })`, §4.3) are REFUSED with a named error.
//    Byte streams are a second controller, a second reader type and an
//    auto-allocation path; half of one is worse than none, because the failure
//    shows up as a wrong `byteLength` far from here.
//  * `ReadableStreamDefaultController.prototype.byobRequest` — same reason.
//  * `WritableStreamDefaultController.prototype.signal` (§5.3) is present and
//    aborts, but the `AbortSignal` it exposes is the runtime's own; there is no
//    reason-carrying abort of an in-flight `write()` beyond that.

(function (g) {
  'use strict';

  if (typeof g.RedReadableCore !== 'function' || typeof g.RedWritableCore !== 'function') {
    throw new Error('streams.js requires the native stream cores (webapi::streams::install)');
  }

  const READABLE = 0, CLOSED = 1, ERRORED = 2;
  const W_WRITABLE = 0, W_ERRORING = 1, W_ERRORED = 2, W_CLOSED = 3;

  function deferred() {
    let resolve, reject;
    const promise = new Promise((res, rej) => { resolve = res; reject = rej; });
    // A rejected `closed`/`ready` nobody is awaiting yet must not surface as an
    // unhandled rejection; the spec marks these handled for exactly this reason.
    promise.catch(() => {});
    return { promise, resolve, reject };
  }

  function validateHighWaterMark(value, fallback) {
    const hwm = value === undefined ? fallback : Number(value);
    if (Number.isNaN(hwm) || hwm < 0) {
      throw new RangeError('highWaterMark must be a non-negative number, got ' + value);
    }
    return hwm;
  }

  function sizeOf(strategy) {
    if (strategy && strategy.size !== undefined) {
      const fn = strategy.size;
      if (typeof fn !== 'function') throw new TypeError('strategy.size must be a function');
      return fn;
    }
    return () => 1;
  }

  // ── ReadableStream ────────────────────────────────────────────────────────

  class ReadableStreamDefaultController {
    constructor() { throw new TypeError('Illegal constructor'); }

    get desiredSize() { return this._core.desiredSize; }

    enqueue(chunk) {
      if (!this._core.canCloseOrEnqueue) {
        throw new TypeError('Cannot enqueue: the stream is closed, closing or errored');
      }
      const stream = this._stream;
      // A waiting reader takes the chunk directly — it never enters the queue,
      // so the size algorithm is not consulted for it (spec §4.9.5).
      if (stream._readRequests.length > 0) {
        const req = stream._readRequests.shift();
        req.resolve({ value: chunk, done: false });
        pullIfNeeded(this);
        return;
      }
      let size;
      try {
        size = this._sizeAlgorithm(chunk);
      } catch (e) {
        errorController(this, e);
        throw e;
      }
      const rc = this._core.enqueue(size);
      if (rc === 1) {
        const e = new RangeError('The queuing strategy returned a size that is not a finite, non-negative number: ' + size);
        errorController(this, e);
        throw e;
      }
      if (rc === 2) throw new TypeError('Cannot enqueue: the stream is closed, closing or errored');
      this._values.push(chunk);
      pullIfNeeded(this);
    }

    close() {
      if (!this._core.canCloseOrEnqueue) {
        throw new TypeError('Cannot close: the stream is already closed, closing or errored');
      }
      // Rust decides whether the close lands now or waits for the queue to
      // drain; closing early would silently truncate the stream.
      if (this._core.requestClose()) closeStream(this._stream);
    }

    error(e) { errorController(this, e); }
  }

  function errorController(controller, reason) {
    if (controller._core.state !== READABLE) return;
    controller._core.resetQueue();
    controller._values.length = 0;
    errorStream(controller._stream, reason);
  }

  function errorStream(stream, reason) {
    if (stream._controller._core.state !== READABLE) return;
    stream._controller._core.state = ERRORED;
    stream._storedError = reason;
    for (const req of stream._readRequests) req.reject(reason);
    stream._readRequests.length = 0;
    if (stream._closedDeferred) stream._closedDeferred.reject(reason);
  }

  function closeStream(stream) {
    const core = stream._controller._core;
    if (core.state !== READABLE) return;
    core.state = CLOSED;
    for (const req of stream._readRequests) req.resolve({ value: undefined, done: true });
    stream._readRequests.length = 0;
    if (stream._closedDeferred) stream._closedDeferred.resolve(undefined);
  }

  function pullIfNeeded(controller) {
    if (!controller._core.beginPull(controller._stream._readRequests.length)) return;
    runPull(controller);
  }

  function runPull(controller) {
    let result;
    try {
      result = controller._pullAlgorithm(controller);
    } catch (e) {
      errorController(controller, e);
      return;
    }
    Promise.resolve(result).then(
      () => {
        // Rust remembered any demand that arrived mid-pull; replaying it is what
        // keeps a stream from stalling with a reader waiting.
        if (controller._core.endPull(controller._stream._readRequests.length)) runPull(controller);
      },
      (e) => errorController(controller, e),
    );
  }

  class ReadableStreamDefaultReader {
    constructor(stream) {
      if (!(stream instanceof ReadableStream)) {
        throw new TypeError('ReadableStreamDefaultReader requires a ReadableStream');
      }
      if (stream.locked) throw new TypeError('This ReadableStream is already locked to a reader');
      this._stream = stream;
      stream._reader = this;
      stream._controller._core.locked = true;
      this._closed = deferred();
      stream._closedDeferred = this._closed;
      const state = stream._controller._core.state;
      if (state === CLOSED) this._closed.resolve(undefined);
      else if (state === ERRORED) this._closed.reject(stream._storedError);
    }

    get closed() { return this._closed.promise; }

    read() {
      const stream = this._stream;
      if (stream === null) {
        return Promise.reject(new TypeError('This reader has been released'));
      }
      const controller = stream._controller;
      const core = controller._core;
      core.disturbed = true;
      if (core.state === ERRORED) return Promise.reject(stream._storedError);
      if (core.queueLength > 0) {
        const chunk = controller._values.shift();
        core.dequeue();
        if (core.closeIfDrained()) closeStream(stream);
        else pullIfNeeded(controller);
        return Promise.resolve({ value: chunk, done: false });
      }
      if (core.state === CLOSED) return Promise.resolve({ value: undefined, done: true });
      const req = deferred();
      stream._readRequests.push(req);
      pullIfNeeded(controller);
      return req.promise;
    }

    cancel(reason) {
      if (this._stream === null) {
        return Promise.reject(new TypeError('This reader has been released'));
      }
      return cancelStream(this._stream, reason);
    }

    releaseLock() {
      const stream = this._stream;
      if (stream === null) return;
      if (stream._readRequests.length > 0) {
        throw new TypeError('Cannot release a reader with pending read requests');
      }
      stream._controller._core.locked = false;
      stream._reader = null;
      stream._closedDeferred = null;
      this._stream = null;
      // Spec: a released reader's `closed` rejects, so an awaiting caller finds
      // out rather than waiting for a promise nothing will ever settle.
      this._closed.reject(new TypeError('This reader has been released'));
    }
  }

  function cancelStream(stream, reason) {
    const controller = stream._controller;
    const core = controller._core;
    core.disturbed = true;
    if (core.state === CLOSED) return Promise.resolve(undefined);
    if (core.state === ERRORED) return Promise.reject(stream._storedError);
    core.resetQueue();
    controller._values.length = 0;
    closeStream(stream);
    let result;
    try {
      result = controller._cancelAlgorithm(reason);
    } catch (e) {
      return Promise.reject(e);
    }
    return Promise.resolve(result).then(() => undefined);
  }

  class ReadableStream {
    constructor(underlyingSource = {}, strategy = {}) {
      const source = underlyingSource || {};
      if (source.type !== undefined) {
        if (String(source.type) === 'bytes') {
          throw new TypeError(
            'ReadableStream: byte streams (type: "bytes") are not implemented in this runtime. ' +
            'A byte stream is a second controller, a second reader type and BYOB auto-allocation; ' +
            'a partial one fails as a wrong byteLength far from the stream. Use a default stream ' +
            'and enqueue Uint8Array chunks.');
        }
        throw new RangeError('ReadableStream: the only defined underlying source type is "bytes"');
      }

      const hwm = validateHighWaterMark(strategy && strategy.highWaterMark, 1);
      this._storedError = undefined;
      this._readRequests = [];
      this._reader = null;
      this._closedDeferred = null;

      const controller = Object.create(ReadableStreamDefaultController.prototype);
      controller._core = new RedReadableCore(hwm);
      controller._values = [];
      controller._stream = this;
      controller._sizeAlgorithm = sizeOf(strategy);
      controller._pullAlgorithm = typeof source.pull === 'function'
        ? (c) => source.pull(c) : () => undefined;
      controller._cancelAlgorithm = typeof source.cancel === 'function'
        ? (r) => source.cancel(r) : () => undefined;
      this._controller = controller;

      let started;
      try {
        started = typeof source.start === 'function' ? source.start(controller) : undefined;
      } catch (e) {
        errorController(controller, e);
        return;
      }
      Promise.resolve(started).then(
        () => {
          controller._core.started = true;
          pullIfNeeded(controller);
        },
        (e) => errorController(controller, e),
      );
    }

    get locked() { return this._controller._core.locked; }

    getReader(options) {
      if (options && options.mode !== undefined) {
        if (String(options.mode) === 'byob') {
          throw new TypeError(
            'ReadableStream.getReader: BYOB readers are not implemented in this runtime — see the ' +
            'byte-stream refusal in the ReadableStream constructor.');
        }
        throw new TypeError('ReadableStream.getReader: the only defined mode is "byob"');
      }
      return new ReadableStreamDefaultReader(this);
    }

    cancel(reason) {
      if (this.locked) return Promise.reject(new TypeError('Cannot cancel a locked ReadableStream'));
      return cancelStream(this, reason);
    }

    tee() {
      const reader = this.getReader();
      let reading = false, canceled1 = false, canceled2 = false;
      let reason1, reason2;
      let c1, c2;
      const cancelDeferred = deferred();

      function pullBoth() {
        if (reading) return;
        reading = true;
        return reader.read().then(
          ({ value, done }) => {
            reading = false;
            if (done) {
              if (!canceled1) c1.close();
              if (!canceled2) c2.close();
              return;
            }
            // Both branches get the SAME value; tee does not clone (spec §4.6).
            if (!canceled1) c1.enqueue(value);
            if (!canceled2) c2.enqueue(value);
          },
          (e) => {
            reading = false;
            if (!canceled1) c1.error(e);
            if (!canceled2) c2.error(e);
          },
        );
      }

      const branch1 = new ReadableStream({
        start(c) { c1 = c; },
        pull: pullBoth,
        cancel(r) {
          canceled1 = true; reason1 = r;
          // The source is only cancelled once BOTH branches have given up.
          if (canceled2) cancelDeferred.resolve(cancelStream(this === undefined ? branch1 : branch1, [reason1, reason2]));
          return cancelDeferred.promise;
        },
      });
      const branch2 = new ReadableStream({
        start(c) { c2 = c; },
        pull: pullBoth,
        cancel(r) {
          canceled2 = true; reason2 = r;
          if (canceled1) cancelDeferred.resolve(reader.cancel([reason1, reason2]));
          return cancelDeferred.promise;
        },
      });
      return [branch1, branch2];
    }

    pipeTo(destination, options = {}) {
      if (!(destination instanceof WritableStream)) {
        return Promise.reject(new TypeError('pipeTo requires a WritableStream'));
      }
      if (this.locked) return Promise.reject(new TypeError('Cannot pipe a locked ReadableStream'));
      if (destination.locked) return Promise.reject(new TypeError('Cannot pipe to a locked WritableStream'));
      const opts = options || {};
      const preventClose = !!opts.preventClose;
      const preventAbort = !!opts.preventAbort;
      const preventCancel = !!opts.preventCancel;
      const signal = opts.signal;

      const reader = this.getReader();
      const writer = destination.getWriter();
      let aborted = false;

      const onAbort = () => {
        aborted = true;
        const reason = (signal && signal.reason) !== undefined ? signal.reason
          : new Error('The pipe was aborted');
        if (!preventAbort) writer.abort(reason);
        if (!preventCancel) reader.cancel(reason);
      };
      if (signal) {
        if (signal.aborted) onAbort();
        else if (typeof signal.addEventListener === 'function') {
          signal.addEventListener('abort', onAbort, { once: true });
        }
      }

      const step = () => {
        if (aborted) return Promise.reject(new Error('The pipe was aborted'));
        // Honouring `ready` is what makes the pipe respect backpressure rather
        // than reading the source as fast as it will go.
        return writer.ready
          .then(() => reader.read())
          .then(({ value, done }) => {
            if (done) return null;
            return writer.write(value).then(step);
          });
      };

      return step().then(
        () => {
          reader.releaseLock();
          if (preventClose) { writer.releaseLock(); return undefined; }
          return writer.close().then(() => { writer.releaseLock(); });
        },
        (e) => {
          if (!preventAbort) { try { writer.abort(e); } catch (_) {} }
          if (!preventCancel) { try { reader.cancel(e); } catch (_) {} }
          try { reader.releaseLock(); } catch (_) {}
          try { writer.releaseLock(); } catch (_) {}
          throw e;
        },
      );
    }

    pipeThrough(transform, options) {
      if (!transform || !(transform.writable instanceof WritableStream) ||
          !(transform.readable instanceof ReadableStream)) {
        throw new TypeError('pipeThrough requires { writable, readable }');
      }
      if (this.locked) throw new TypeError('Cannot pipe a locked ReadableStream');
      // The returned promise is intentionally not awaited (spec §4.5): errors
      // surface on the readable end. Marking it handled keeps a piped error from
      // becoming an unhandled rejection as well as a stream error.
      this.pipeTo(transform.writable, options).catch(() => {});
      return transform.readable;
    }

    values(options) {
      const reader = this.getReader();
      const preventCancel = !!(options && options.preventCancel);
      return {
        next() {
          return reader.read().then((r) => {
            if (r.done) reader.releaseLock();
            return r;
          });
        },
        return(value) {
          if (!preventCancel) reader.cancel(value);
          reader.releaseLock();
          return Promise.resolve({ value, done: true });
        },
        [Symbol.asyncIterator]() { return this; },
      };
    }
  }
  ReadableStream.prototype[Symbol.asyncIterator] = ReadableStream.prototype.values;

  // ── WritableStream ────────────────────────────────────────────────────────

  class WritableStreamDefaultController {
    constructor() { throw new TypeError('Illegal constructor'); }
    get signal() { return this._abortController ? this._abortController.signal : undefined; }
    error(e) {
      if (this._core.state === W_WRITABLE) startErroring(this._stream, e);
    }
  }

  function startErroring(stream, reason) {
    const core = stream._core;
    if (core.state !== W_WRITABLE) return;
    core.state = W_ERRORING;
    stream._storedError = reason;
    if (stream._writer) stream._writer._rejectReady(reason);
    if (!core.inFlightWrite && !core.inFlightClose) finishErroring(stream);
  }

  function finishErroring(stream) {
    const core = stream._core;
    core.state = W_ERRORED;
    core.resetQueue();
    for (const req of stream._writeRequests) req.reject(stream._storedError);
    stream._writeRequests.length = 0;
    stream._chunks.length = 0;
    if (stream._closeRequest) {
      stream._closeRequest.reject(stream._storedError);
      stream._closeRequest = null;
    }
    if (stream._writer) stream._writer._rejectClosed(stream._storedError);
    const pending = stream._pendingAbort;
    stream._pendingAbort = null;
    if (pending) {
      let result;
      try {
        result = stream._controller._abortAlgorithm(pending.reason);
      } catch (e) {
        pending.reject(e);
        return;
      }
      Promise.resolve(result).then(() => pending.resolve(undefined), (e) => pending.reject(e));
    }
  }

  function advanceQueue(stream) {
    const core = stream._core;
    if (!core.started || core.inFlightWrite || core.inFlightClose) return;
    if (core.state === W_ERRORING) { finishErroring(stream); return; }
    if (core.queueLength === 0) {
      if (core.closeRequested && stream._closeRequest) processClose(stream);
      return;
    }
    const chunk = stream._chunks.shift();
    const request = stream._writeRequests.shift();
    core.inFlightWrite = true;
    let result;
    try {
      result = stream._controller._writeAlgorithm(chunk, stream._controller);
    } catch (e) {
      core.inFlightWrite = false;
      core.dequeue();
      request.reject(e);
      startErroring(stream, e);
      return;
    }
    Promise.resolve(result).then(
      () => {
        core.inFlightWrite = false;
        core.dequeue();
        if (core.updateBackpressure() && stream._writer) stream._writer._syncReady();
        request.resolve(undefined);
        advanceQueue(stream);
      },
      (e) => {
        core.inFlightWrite = false;
        core.dequeue();
        request.reject(e);
        startErroring(stream, e);
      },
    );
  }

  function processClose(stream) {
    const core = stream._core;
    core.inFlightClose = true;
    const request = stream._closeRequest;
    stream._closeRequest = null;
    let result;
    try {
      // A close algorithm that throws SYNCHRONOUSLY has to land here, not
      // escape into whatever reaction called advanceQueue. When it escaped, the
      // close request was never settled and the stream hung forever with no
      // error anywhere — which is exactly how a truncated DecompressionStream
      // presented before this guard existed.
      result = stream._controller._closeAlgorithm();
    } catch (e) {
      core.inFlightClose = false;
      request.reject(e);
      startErroring(stream, e);
      return;
    }
    Promise.resolve(result).then(
      () => {
        core.inFlightClose = false;
        core.state = W_CLOSED;
        request.resolve(undefined);
        if (stream._writer) stream._writer._resolveClosed();
      },
      (e) => {
        core.inFlightClose = false;
        request.reject(e);
        startErroring(stream, e);
      },
    );
  }

  class WritableStreamDefaultWriter {
    constructor(stream) {
      if (!(stream instanceof WritableStream)) {
        throw new TypeError('WritableStreamDefaultWriter requires a WritableStream');
      }
      if (stream.locked) throw new TypeError('This WritableStream is already locked to a writer');
      this._stream = stream;
      stream._writer = this;
      stream._core.locked = true;
      this._closedD = deferred();
      this._readyD = deferred();
      const state = stream._core.state;
      if (state === W_ERRORED || state === W_ERRORING) {
        this._closedD.reject(stream._storedError);
        this._readyD.reject(stream._storedError);
      } else if (state === W_CLOSED) {
        this._closedD.resolve(undefined);
        this._readyD.resolve(undefined);
      } else if (!stream._core.backpressure) {
        this._readyD.resolve(undefined);
      }
    }

    get closed() { return this._closedD.promise; }
    get ready() { return this._readyD.promise; }
    get desiredSize() {
      if (this._stream === null) throw new TypeError('This writer has been released');
      const core = this._stream._core;
      if (core.state === W_ERRORED || core.state === W_ERRORING) return null;
      if (core.state === W_CLOSED) return 0;
      return core.desiredSize;
    }

    _syncReady() {
      const core = this._stream._core;
      if (core.backpressure) this._readyD = deferred();
      else this._readyD.resolve(undefined);
    }
    _rejectReady(e) {
      this._readyD.reject(e);
      this._readyD = deferred();
      this._readyD.reject(e);
    }
    _rejectClosed(e) { this._closedD.reject(e); }
    _resolveClosed() { this._closedD.resolve(undefined); }

    write(chunk) {
      const stream = this._stream;
      if (stream === null) return Promise.reject(new TypeError('This writer has been released'));
      const core = stream._core;
      if (core.state === W_ERRORED || core.state === W_ERRORING) {
        return Promise.reject(stream._storedError);
      }
      if (core.state === W_CLOSED || core.closeRequested) {
        return Promise.reject(new TypeError('Cannot write to a closing or closed WritableStream'));
      }
      let size;
      try {
        size = stream._controller._sizeAlgorithm(chunk);
      } catch (e) {
        startErroring(stream, e);
        return Promise.reject(e);
      }
      if (core.enqueue(size) === 1) {
        const e = new RangeError('The queuing strategy returned a size that is not a finite, non-negative number: ' + size);
        startErroring(stream, e);
        return Promise.reject(e);
      }
      stream._chunks.push(chunk);
      const request = deferred();
      stream._writeRequests.push(request);
      if (core.updateBackpressure()) this._syncReady();
      advanceQueue(stream);
      return request.promise;
    }

    close() {
      const stream = this._stream;
      if (stream === null) return Promise.reject(new TypeError('This writer has been released'));
      const core = stream._core;
      if (core.closeRequested || core.state === W_CLOSED) {
        return Promise.reject(new TypeError('This WritableStream is already closing or closed'));
      }
      if (core.state === W_ERRORED || core.state === W_ERRORING) {
        return Promise.reject(stream._storedError);
      }
      core.closeRequested = true;
      const request = deferred();
      stream._closeRequest = request;
      // Releasing `ready` on close is what lets `await writer.ready` after a
      // close settle instead of hanging forever.
      if (core.backpressure) { core.updateBackpressure(); this._readyD.resolve(undefined); }
      advanceQueue(stream);
      return request.promise;
    }

    abort(reason) {
      const stream = this._stream;
      if (stream === null) return Promise.reject(new TypeError('This writer has been released'));
      return abortStream(stream, reason);
    }

    releaseLock() {
      const stream = this._stream;
      if (stream === null) return;
      stream._core.locked = false;
      stream._writer = null;
      this._stream = null;
      const e = new TypeError('This writer has been released');
      this._readyD.reject(e);
      this._closedD.reject(e);
    }
  }

  function abortStream(stream, reason) {
    const core = stream._core;
    if (core.state === W_CLOSED || core.state === W_ERRORED) return Promise.resolve(undefined);
    if (stream._pendingAbort) return stream._pendingAbort.promise;
    const request = deferred();
    stream._pendingAbort = { promise: request.promise, resolve: request.resolve, reject: request.reject, reason };
    startErroring(stream, reason !== undefined ? reason : new TypeError('The stream was aborted'));
    return request.promise;
  }

  class WritableStream {
    constructor(underlyingSink = {}, strategy = {}) {
      const sink = underlyingSink || {};
      if (sink.type !== undefined) {
        throw new RangeError('WritableStream: an underlying sink has no defined `type`');
      }
      const hwm = validateHighWaterMark(strategy && strategy.highWaterMark, 1);
      this._core = new RedWritableCore(hwm);
      this._chunks = [];
      this._writeRequests = [];
      this._closeRequest = null;
      this._pendingAbort = null;
      this._storedError = undefined;
      this._writer = null;

      const controller = Object.create(WritableStreamDefaultController.prototype);
      controller._core = this._core;
      controller._stream = this;
      controller._sizeAlgorithm = sizeOf(strategy);
      controller._writeAlgorithm = typeof sink.write === 'function'
        ? (chunk, c) => sink.write(chunk, c) : () => undefined;
      controller._closeAlgorithm = typeof sink.close === 'function'
        ? () => sink.close() : () => undefined;
      controller._abortAlgorithm = typeof sink.abort === 'function'
        ? (r) => sink.abort(r) : () => undefined;
      controller._abortController = typeof g.AbortController === 'function' ? new g.AbortController() : null;
      this._controller = controller;

      let started;
      try {
        started = typeof sink.start === 'function' ? sink.start(controller) : undefined;
      } catch (e) {
        startErroring(this, e);
        return;
      }
      Promise.resolve(started).then(
        () => {
          this._core.started = true;
          if (this._core.updateBackpressure() && this._writer) this._writer._syncReady();
          advanceQueue(this);
        },
        (e) => { this._core.started = true; startErroring(this, e); },
      );
    }

    get locked() { return this._core.locked; }
    getWriter() { return new WritableStreamDefaultWriter(this); }
    abort(reason) {
      if (this.locked) return Promise.reject(new TypeError('Cannot abort a locked WritableStream'));
      return abortStream(this, reason);
    }
    close() {
      if (this.locked) return Promise.reject(new TypeError('Cannot close a locked WritableStream'));
      const writer = this.getWriter();
      const p = writer.close();
      // Keep the lock for the duration; releasing here would reject `p`.
      return p.then(
        (v) => { writer.releaseLock(); return v; },
        (e) => { writer.releaseLock(); throw e; },
      );
    }
  }

  // ── TransformStream ───────────────────────────────────────────────────────

  class TransformStreamDefaultController {
    constructor() { throw new TypeError('Illegal constructor'); }
    get desiredSize() { return this._readableController.desiredSize; }
    enqueue(chunk) { this._readableController.enqueue(chunk); }
    error(e) {
      this._readableController.error(e);
      if (this._stream._writable._core.state === W_WRITABLE) startErroring(this._stream._writable, e);
    }
    terminate() {
      try { this._readableController.close(); } catch (_) {}
      const w = this._stream._writable;
      if (w._core.state === W_WRITABLE) {
        startErroring(w, new TypeError('The transform stream was terminated'));
      }
    }
  }

  class TransformStream {
    constructor(transformer = {}, writableStrategy = {}, readableStrategy = {}) {
      const t = transformer || {};
      if (t.readableType !== undefined || t.writableType !== undefined) {
        throw new RangeError('TransformStream: readableType/writableType are reserved and must be undefined');
      }
      const controller = Object.create(TransformStreamDefaultController.prototype);
      controller._stream = this;

      this._readable = new ReadableStream({
        start: (c) => { controller._readableController = c; },
      }, readableStrategy);

      const transformAlgorithm = typeof t.transform === 'function'
        ? (chunk) => t.transform(chunk, controller)
        // Spec §"identity transform": with no `transform`, chunks pass through.
        : (chunk) => { controller.enqueue(chunk); };
      const flushAlgorithm = typeof t.flush === 'function' ? () => t.flush(controller) : () => undefined;

      this._writable = new WritableStream({
        write: (chunk) => Promise.resolve(transformAlgorithm(chunk)).catch((e) => {
          controller.error(e);
          throw e;
        }),
        close: () => {
          // `flush` may throw synchronously — a native codec reporting a
          // truncated stream does exactly that. Error the READABLE end first, so
          // whoever is reading finds out, then let the rejection travel.
          let flushed;
          try {
            flushed = flushAlgorithm();
          } catch (e) {
            controller._readableController.error(e);
            throw e;
          }
          return Promise.resolve(flushed).then(
            () => { try { controller._readableController.close(); } catch (_) {} },
            (e) => { controller._readableController.error(e); throw e; },
          );
        },
        abort: (reason) => { controller._readableController.error(reason); },
      }, writableStrategy);

      this._controller = controller;
      if (typeof t.start === 'function') t.start(controller);
    }

    get readable() { return this._readable; }
    get writable() { return this._writable; }
  }

  // ── Queuing strategies ────────────────────────────────────────────────────

  class CountQueuingStrategy {
    constructor(init) {
      if (!init || init.highWaterMark === undefined) {
        throw new TypeError('CountQueuingStrategy requires { highWaterMark }');
      }
      this._hwm = Number(init.highWaterMark);
    }
    get highWaterMark() { return this._hwm; }
    // Spec: `size` is a getter returning a function that is NOT bound to the
    // instance, so `strategy.size` can be pulled off and called standalone.
    get size() { return () => 1; }
  }

  class ByteLengthQueuingStrategy {
    constructor(init) {
      if (!init || init.highWaterMark === undefined) {
        throw new TypeError('ByteLengthQueuingStrategy requires { highWaterMark }');
      }
      this._hwm = Number(init.highWaterMark);
    }
    get highWaterMark() { return this._hwm; }
    get size() { return (chunk) => chunk.byteLength; }
  }

  // ── CompressionStream / DecompressionStream ───────────────────────────────

  function makeCodecStream(format, decompress) {
    const zip = new g.RedZip(format, decompress);
    return new TransformStream({
      transform(chunk, controller) {
        const out = zip.push(chunk);
        // A deflate block is not emitted until it is worth emitting, so most
        // pushes legitimately produce nothing. Enqueueing an empty chunk would
        // make every consumer see spurious zero-length reads.
        if (out.byteLength > 0) controller.enqueue(out);
      },
      flush(controller) {
        const out = zip.finish();
        if (out.byteLength > 0) controller.enqueue(out);
      },
    });
  }

  class CompressionStream {
    constructor(format) {
      const t = makeCodecStream(String(format), false);
      this._t = t;
    }
    get readable() { return this._t.readable; }
    get writable() { return this._t.writable; }
  }

  class DecompressionStream {
    constructor(format) {
      const t = makeCodecStream(String(format), true);
      this._t = t;
    }
    get readable() { return this._t.readable; }
    get writable() { return this._t.writable; }
  }

  // ── Publish ───────────────────────────────────────────────────────────────

  const exports = {
    ReadableStream, ReadableStreamDefaultReader, ReadableStreamDefaultController,
    WritableStream, WritableStreamDefaultWriter, WritableStreamDefaultController,
    TransformStream, TransformStreamDefaultController,
    CountQueuingStrategy, ByteLengthQueuingStrategy,
    CompressionStream, DecompressionStream,
  };
  for (const name of Object.keys(exports)) {
    // WebIDL: every interface prototype says its name to
    // `Object.prototype.toString` (webapi/interface_tags.rs).
    Object.defineProperty(exports[name].prototype, Symbol.toStringTag, { value: name, configurable: true });
    Object.defineProperty(g, name, { value: exports[name], writable: true, configurable: true });
  }

  // `Blob.prototype.stream` is native (webapi/blob.rs): 64 KiB chunks, and a
  // path-backed File read off the JS thread. It builds its stream from a
  // factory made right after this script runs.
})(globalThis);
