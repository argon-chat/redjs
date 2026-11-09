//! Native `Blob` (W3C File API): an immutable window onto bytes that live in
//! memory OR in a file on disk, with spec-shaped `slice`, the promise reads
//! (`arrayBuffer` / `bytes` / `text`), `stream()`, `size` / `type`, and the
//! constructor from BlobParts. `File` (a Blob with a name) is [`super::file`].
//!
//! # Three sources
//!
//! - [`Source::Memory`] — an `Arc<Vec<u8>>`: script-made Blobs, response
//!   bodies, values read back from IndexedDB. Reads resolve at once.
//! - [`Source::Disk`] — a path plus the size and mtime the host stat'ed when it
//!   made the `File` (a picker, a drop, `appFs.openFile`). Nothing is read
//!   until a consumer asks; then the read runs on a [read thread](read_async)
//!   (at most [`MAX_READS_IN_FLIGHT`], the rest queue) and settles from the
//!   pump. **Chromium's snapshot rule:** if the file's size or mtime is no
//!   longer what it was, the read fails with `NotReadableError` and Chromium's
//!   own message, rather than handing over bytes the user never picked.
//! - [`Source::Unreadable`] — a Blob assembled from a File that could not be
//!   read any more when it was built: every read is that failure, which is
//!   what a browser's lazy composite blob gives the same code later.
//!
//! `slice()` stays lazy on all three — it is a new window onto the same source,
//! never a copy.
//!
//! # The synchronous consumers (A-F2)
//!
//! A fetch/Request/Response body, FormData's multipart encoding,
//! `WebSocket.send`, a value serialized for storage (IndexedDB) and a
//! multi-part `new Blob([file, …])` take the bytes NOW, through
//! [`bytes_of`]. On a path-backed Blob that is a read on the UI thread;
//! past 16 MiB it says so once per consumer, naming `stream()`. Making the
//! fetch body async is phase 2 of the design. (`createImageBitmap` and an
//! `Image`'s `src` read a path-backed Blob asynchronously, like `text()`.)

use core::ffi::c_void;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use super::util::*;
use crate::glue;

// ---------------------------------------------------------------------------
// Data
// ---------------------------------------------------------------------------

/// A file on disk as it was when its `File` was made — what every later read
/// is checked against (Chromium: "snapshot state").
#[derive(Debug, PartialEq, Eq)]
pub struct DiskFile {
    pub path: PathBuf,
    pub size: u64,
    /// `None` only on a filesystem that keeps no modification time.
    pub mtime: Option<SystemTime>,
}

/// Why a read failed — the two File API errors a read can end in, plus the
/// one that is really neither.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadError {
    /// The file is gone.
    NotFound,
    /// It changed since the `File` was made, or cannot be opened or read.
    NotReadable,
    /// The buffer for the bytes could not be allocated. A READ reports it as
    /// `NotReadableError`, which is what Chromium's FileReaderLoader does
    /// when its ArrayBuffer cannot be made; a consumer with a better word
    /// for it says that instead (the serializer: DataCloneError, out of
    /// memory).
    OutOfMemory,
}

impl ReadError {
    pub fn name(self) -> &'static str {
        match self {
            ReadError::NotFound => "NotFoundError",
            ReadError::NotReadable | ReadError::OutOfMemory => "NotReadableError",
        }
    }

    /// Chromium's messages (`file_error::CreateDOMException`), verbatim.
    pub fn message(self) -> &'static str {
        match self {
            ReadError::NotFound => "A requested file or directory could not be found at the time an operation was processed.",
            ReadError::NotReadable | ReadError::OutOfMemory => {
                "The requested file could not be read, typically due to permission problems that have occurred after a reference to a file was acquired."
            }
        }
    }

    /// The DOMException a promise rejects with.
    ///
    /// # Safety
    /// `g` must be a live context, on its thread.
    pub unsafe fn to_js(self, g: *mut c_void) -> u64 {
        super::domexception::create(g, self.name(), self.message())
    }
}

/// Where a Blob's bytes are.
#[derive(Clone, Debug)]
pub enum Source {
    Memory(Arc<Vec<u8>>),
    Disk(Arc<DiskFile>),
    Unreadable(ReadError),
}

/// What a `File` adds to a Blob.
#[derive(Clone, Debug, PartialEq)]
pub struct FileMeta {
    pub name: String,
    /// Milliseconds since the epoch, integral (`long long`).
    pub last_modified: f64,
    /// `webkitRelativePath`: `""` unless the File came from a directory pick.
    pub relative_path: String,
    /// Made by the host from a file on disk ([`super::file::create_file`]),
    /// not by script — what `getPathForFile` answers for.
    pub backed: bool,
}

/// A Blob instance's data. A `File` is the same host class with `file` set —
/// so every Blob method's brand check accepts a File, as the IDL inheritance
/// says it must.
pub struct BlobData {
    pub source: Source,
    pub offset: u64,
    pub len: u64,
    pub mime: String,
    pub file: Option<FileMeta>,
}

impl BlobData {
    pub fn range(&self) -> Range {
        Range { source: self.source.clone(), offset: self.offset, len: self.len }
    }
}

/// A source and a window onto it, with no JS anywhere: `Send`. What a read
/// thread, the blob URL store and the serializer's attachments carry.
#[derive(Clone, Debug)]
pub struct Range {
    pub source: Source,
    pub offset: u64,
    pub len: u64,
}

impl Range {
    /// The bytes, when they are already in memory (no read needed).
    pub fn in_memory(&self) -> Option<&[u8]> {
        match &self.source {
            Source::Memory(b) => {
                let (o, n) = (self.offset as usize, self.len as usize);
                b.get(o..o.checked_add(n)?)
            }
            _ => None,
        }
    }

    /// The bytes of a memory window, where they are (no copy); the failure
    /// of an Unreadable one. A Disk range is not in memory: read it.
    pub fn read_in_place(&self) -> Result<&[u8], ReadError> {
        match &self.source {
            Source::Unreadable(e) => Err(*e),
            _ => self.in_memory().ok_or(ReadError::NotReadable),
        }
    }

    /// Does the window fit inside its source? (The serializer's decode check.)
    pub fn fits(&self) -> bool {
        let Some(end) = self.offset.checked_add(self.len) else { return false };
        match &self.source {
            Source::Memory(b) => end <= b.len() as u64,
            Source::Disk(f) => end <= f.size,
            Source::Unreadable(_) => true,
        }
    }

    /// Reads the bytes on THIS thread: a copy of a memory window, or the
    /// snapshot check and the read of a file range.
    pub fn read(&self) -> Result<Vec<u8>, ReadError> {
        let mut out = Vec::new();
        self.read_into(&mut out)?;
        Ok(out)
    }

    /// [`Range::read`], appending to `out` (the serializer writes a stored
    /// File straight into its buffer, with no second copy). A buffer that
    /// cannot grow is [`ReadError::OutOfMemory`], never an abort; on any
    /// error `out` may hold part of the bytes.
    pub fn read_into(&self, out: &mut Vec<u8>) -> Result<(), ReadError> {
        match &self.source {
            Source::Memory(_) => {
                let b = self.in_memory().ok_or(ReadError::NotReadable)?;
                out.try_reserve_exact(b.len()).map_err(|_| ReadError::OutOfMemory)?;
                out.extend_from_slice(b);
                Ok(())
            }
            Source::Disk(f) => read_disk(f, self.offset, self.len, out),
            Source::Unreadable(e) => Err(*e),
        }
    }

    /// Writes the bytes into `out` on THIS thread without holding them all
    /// in memory: a memory window in one write, a file range streamed in
    /// pieces under the same snapshot check [`Range::read`] makes. A host
    /// that copies a picked file somewhere (vue-native's
    /// `FileSystemWritableFileStream.write(file)`, `<a download>`) uses it
    /// on its own worker, so a large file is neither read on the UI thread
    /// nor loaded whole. On any error `out` may hold part of the bytes.
    pub fn copy_into(&self, out: &mut dyn std::io::Write) -> Result<(), CopyError> {
        match &self.source {
            Source::Memory(_) => {
                let b = self.in_memory().ok_or(CopyError::Read(ReadError::NotReadable))?;
                out.write_all(b).map_err(CopyError::Write)
            }
            Source::Disk(f) => copy_disk(f, self.offset, self.len, out),
            Source::Unreadable(e) => Err(CopyError::Read(*e)),
        }
    }
}

/// Why [`Range::copy_into`] stopped: the source could not be read (the File
/// API's errors), or the destination refused the bytes (the caller's own
/// I/O error, untouched, so it can map a full disk to its own name).
#[derive(Debug)]
pub enum CopyError {
    Read(ReadError),
    Write(std::io::Error),
}

/// [`read_disk`]'s snapshot check, then the range in 64 KiB pieces. A file
/// that ends before the range does is `NotReadable`, as a short read is.
fn copy_disk(f: &DiskFile, offset: u64, len: u64, out: &mut dyn std::io::Write) -> Result<(), CopyError> {
    use std::io::{Read, Seek, SeekFrom};
    let read = |e: std::io::Error| CopyError::Read(io_error(e));
    let mut file = std::fs::File::open(&f.path).map_err(read)?;
    let meta = file.metadata().map_err(read)?;
    if meta.is_dir() || meta.len() != f.size || meta.modified().ok() != f.mtime {
        return Err(CopyError::Read(ReadError::NotReadable));
    }
    if offset.checked_add(len).is_none_or(|end| end > f.size) {
        return Err(CopyError::Read(ReadError::NotReadable));
    }
    file.seek(SeekFrom::Start(offset)).map_err(read)?;
    let mut left = len;
    let mut buf = vec![0u8; 64 << 10];
    while left > 0 {
        let want = left.min(buf.len() as u64) as usize;
        let n = file.read(&mut buf[..want]).map_err(read)?;
        if n == 0 {
            return Err(CopyError::Read(ReadError::NotReadable));
        }
        out.write_all(&buf[..n]).map_err(CopyError::Write)?;
        left -= n as u64;
    }
    Ok(())
}

fn io_error(e: std::io::Error) -> ReadError {
    match e.kind() {
        std::io::ErrorKind::NotFound => ReadError::NotFound,
        _ => ReadError::NotReadable,
    }
}

/// The snapshot check, then the read. The open shares read, write and delete
/// (std's default on Windows), so a reader never blocks the app that owns the
/// file.
fn read_disk(f: &DiskFile, offset: u64, len: u64, out: &mut Vec<u8>) -> Result<(), ReadError> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(&f.path).map_err(io_error)?;
    let meta = file.metadata().map_err(io_error)?;
    if meta.is_dir() || meta.len() != f.size || meta.modified().ok() != f.mtime {
        return Err(ReadError::NotReadable);
    }
    if offset.checked_add(len).is_none_or(|end| end > f.size) {
        return Err(ReadError::NotReadable);
    }
    let len = usize::try_from(len).map_err(|_| ReadError::OutOfMemory)?;
    out.try_reserve_exact(len).map_err(|_| ReadError::OutOfMemory)?;
    let at = out.len();
    file.seek(SeekFrom::Start(offset)).map_err(io_error)?;
    file.take(len as u64).read_to_end(out).map_err(io_error)?;
    if out.len() - at != len {
        return Err(ReadError::NotReadable);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Read threads
// ---------------------------------------------------------------------------

/// Path-backed reads running at once. More than this QUEUE rather than fail:
/// a browser never refuses a read. (This is not fetch's cap: fetch allows 64
/// network requests and REJECTS past that.)
///
/// The pool is one per process. Eight reads that block for a long time, such
/// as a file on an offline network share or a OneDrive placeholder being
/// downloaded, therefore hold up every other path-backed read in every
/// context until one of them returns. Nothing is lost; the rest queue.
pub const MAX_READS_IN_FLIGHT: usize = 8;

type Job = Box<dyn FnOnce() + Send>;

struct Pool {
    jobs: VecDeque<Job>,
    running: usize,
    /// The most that ever ran at once (tests hold the cap to it).
    peak: usize,
}

fn pool() -> &'static Mutex<Pool> {
    static P: OnceLock<Mutex<Pool>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(Pool { jobs: VecDeque::new(), running: 0, peak: 0 }))
}

/// Runs one job. A job that panics loses its own result, never the thread:
/// otherwise each panic would retire a slot for good, and after eight of them
/// no path-backed read in the process would ever run again. (The read jobs
/// also catch their own panic, so their caller still gets an answer.)
fn run_job(job: Job) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
}

/// A read thread: runs jobs until the queue is empty, then leaves. The exit
/// decision and the decrement happen under the same lock a submitter takes,
/// so a job is never left behind by a thread that was on its way out.
fn worker(first: Arc<Mutex<Option<Job>>>) {
    if let Some(job) = first.lock().unwrap().take() {
        run_job(job);
    }
    loop {
        let job = {
            let mut p = pool().lock().unwrap();
            match p.jobs.pop_front() {
                Some(j) => j,
                None => {
                    p.running -= 1;
                    return;
                }
            }
        };
        run_job(job);
    }
}

fn submit(job: Job) {
    {
        let mut p = pool().lock().unwrap();
        if p.running >= MAX_READS_IN_FLIGHT {
            p.jobs.push_back(job);
            return; // a running thread takes it next
        }
        p.running += 1;
        p.peak = p.peak.max(p.running);
    }
    // The new thread's first job travels beside it rather than through the
    // queue, so that a failed spawn can take back exactly this job.
    let first = Arc::new(Mutex::new(Some(job)));
    let theirs = Arc::clone(&first);
    if std::thread::Builder::new().name("redjs-blob-read".into()).spawn(move || worker(theirs)).is_err() {
        pool().lock().unwrap().running -= 1;
        // No thread to be had: run THIS job here, and only it. Other contexts'
        // reads stay queued for the threads that exist (a job is queued only
        // while all MAX_READS_IN_FLIGHT of them run). The result is still
        // delivered from the pump, so the caller still sees an asynchronous
        // read.
        let job = first.lock().unwrap().take();
        if let Some(job) = job {
            run_job(job);
        }
    }
}

#[cfg(test)]
pub(crate) fn peak_reads_in_flight() -> usize {
    pool().lock().unwrap().peak
}

/// The context key crosses to the read thread only to name the queue the
/// result goes back to — never dereferenced there (fetch's `Ctx`).
#[derive(Clone, Copy)]
struct Ctx(*mut c_void);
unsafe impl Send for Ctx {}

impl Ctx {
    fn ptr(self) -> *mut c_void {
        self.0
    }
}

/// What a read thread hands back to the JS thread.
pub type ReadDone = Box<dyn FnOnce(*mut c_void, Result<Vec<u8>, ReadError>) + Send>;

/// Reads `range` off the JS thread and runs `done` ON it, from the pump,
/// through the context's [inbox](Inbox): inside the pump's delivery budget,
/// so an awaited chain of reads does not wait a pump (a frame) per read.
///
/// A read can outlive its context, and the address can be handed to a new
/// one in the meantime; `done` carries values protected in the OLD heap, so
/// it is dropped unrun, and its bytes freed, once that context is gone.
pub(crate) fn read_async(g: *mut c_void, range: Range, done: ReadDone) {
    let ctx = Ctx(g);
    let inbox = inbox(g);
    if let Some(i) = &inbox {
        i.expect();
    }
    submit(Box::new(move || {
        // A read that panics is a failed read, and its caller still hears so.
        let r = std::panic::catch_unwind(|| range.read()).unwrap_or(Err(ReadError::NotReadable));
        match inbox {
            Some(i) => i.push(Box::new(move |g| done(g, r))),
            // A context that was never registered has no id to guard with:
            // the plain native queue, and only while it is still unregistered.
            None => crate::web::schedule_native(
                ctx.ptr(),
                0.0,
                Box::new(move |g| {
                    if crate::context::id_of(g).is_none() {
                        done(g, r);
                    }
                }),
            ),
        }
    }));
}

/// Runs `task` on the JS thread as a task of its own (never synchronously),
/// through the same inbox: in this pump's delivery phase if that has not run
/// yet, else in the next pump's. `getAsString`'s callback and an `<img>`
/// load of in-memory bytes go this way.
pub(crate) fn queue_task(g: *mut c_void, task: Task) {
    match inbox(g) {
        Some(i) => {
            i.expect();
            i.push(task);
        }
        None => crate::web::schedule_native(g, 0.0, task),
    }
}

// ---------------------------------------------------------------------------
// Delivery: back on the JS thread, inside the pump's budget
// ---------------------------------------------------------------------------
//
// A read's result goes into its context's inbox, and the read thread wakes
// the context (`web::wake`). The inbox is a PUMP SOURCE (`web::PumpSource`),
// the seam IndexedDB delivers through. While a read is outstanding the pump
// keeps delivering, and waits on the waker, within its one bounded budget.
// Measured by `awaited_path_backed_reads_do_not_cost_a_pump_each`: 200
// awaited slice reads took 201 pumps through `web::schedule_native`, which a
// pump drains only once, at its start.
//
// The price is IndexedDB's too: while a long read is in flight, each pump
// spends up to its budget waiting for it.

/// One task for the JS thread.
pub type Task = Box<dyn FnOnce(*mut c_void) + Send>;

/// A context's deliveries. Keyed by address in [`inboxes`], made with the
/// context's id, and retired at its teardown; a read thread holds its own
/// `Arc`, so a late result lands in a retired inbox and is dropped, never in
/// a newer context that took the address.
struct Inbox {
    id: crate::context::ContextId,
    /// Tasks promised and not yet run: reads in flight plus queued results.
    pending: AtomicUsize,
    /// Of those, the ones in `q`.
    queued: AtomicUsize,
    q: Mutex<VecDeque<Task>>,
    /// Set under `q`'s lock by [`retire`].
    gone: AtomicBool,
}

/// Every inbox's `pending`, summed: with nothing outstanding anywhere, the
/// pump source costs one atomic load.
static PENDING: AtomicUsize = AtomicUsize::new(0);

impl Inbox {
    /// One more task will come.
    fn expect(&self) {
        self.pending.fetch_add(1, Ordering::AcqRel);
        PENDING.fetch_add(1, Ordering::AcqRel);
    }

    /// Hands a promised task over, from any thread. Dropped unrun when the
    /// context is gone.
    fn push(&self, task: Task) {
        let mut q = self.q.lock().unwrap();
        if self.gone.load(Ordering::Acquire) {
            drop(q);
            drop(task);
            return;
        }
        q.push_back(task);
        self.queued.fetch_add(1, Ordering::AcqRel);
        drop(q);
        crate::web::wake(self.id);
    }
}

fn inboxes() -> &'static Mutex<HashMap<usize, Arc<Inbox>>> {
    static I: OnceLock<Mutex<HashMap<usize, Arc<Inbox>>>> = OnceLock::new();
    I.get_or_init(|| Mutex::new(HashMap::new()))
}

/// `g`'s inbox, made on first use; `None` for a context nobody registered.
fn inbox(g: *mut c_void) -> Option<Arc<Inbox>> {
    let id = crate::context::id_of(g)?;
    let (inbox, stale) = {
        let mut m = inboxes().lock().unwrap();
        match m.get(&(g as usize)) {
            Some(i) if i.id == id => return Some(Arc::clone(i)),
            _ => {
                let fresh = Arc::new(Inbox {
                    id,
                    pending: AtomicUsize::new(0),
                    queued: AtomicUsize::new(0),
                    q: Mutex::new(VecDeque::new()),
                    gone: AtomicBool::new(false),
                });
                (Arc::clone(&fresh), m.insert(g as usize, fresh))
            }
        }
    };
    if let Some(old) = stale {
        retire(&old);
    }
    Some(inbox)
}

fn lookup(g: *mut c_void) -> Option<Arc<Inbox>> {
    inboxes().lock().unwrap().get(&(g as usize)).cloned()
}

/// The context is gone: what is queued is dropped (and its bytes freed), and
/// what is still in flight will be dropped when it lands.
fn retire(i: &Inbox) {
    let dropped = {
        let mut q = i.q.lock().unwrap();
        i.gone.store(true, Ordering::Release);
        i.queued.store(0, Ordering::Release);
        PENDING.fetch_sub(i.pending.swap(0, Ordering::AcqRel), Ordering::AcqRel);
        std::mem::take(&mut *q)
    };
    drop(dropped);
}

unsafe fn src_inflight(g: *mut c_void) -> bool {
    PENDING.load(Ordering::Acquire) != 0 && lookup(g).is_some_and(|i| i.pending.load(Ordering::Acquire) != 0)
}

unsafe fn src_ready(g: *mut c_void) -> bool {
    PENDING.load(Ordering::Acquire) != 0 && lookup(g).is_some_and(|i| i.queued.load(Ordering::Acquire) != 0)
}

/// Runs what is queued, one task each with a checkpoint after it, within the
/// pump's budget (0 = only what was queued when it started).
unsafe fn src_drain(g: *mut c_void) -> usize {
    if PENDING.load(Ordering::Acquire) == 0 {
        return 0;
    }
    let Some(inbox) = lookup(g) else { return 0 };
    if inbox.queued.load(Ordering::Acquire) == 0 {
        return 0;
    }
    if crate::context::id_of(g) != Some(inbox.id) {
        retire(&inbox);
        return 0;
    }
    // Microtasks left by earlier tasks run before this one: HTML's order.
    crate::web::checkpoint(g);
    let budget_us = crate::web::pump_budget_us();
    let cap = if budget_us == 0 { inbox.queued.load(Ordering::Acquire) } else { usize::MAX };
    let start = std::time::Instant::now();
    let mut n = 0usize;
    while n < cap {
        let Some(task) = inbox.q.lock().unwrap().pop_front() else { break };
        inbox.queued.fetch_sub(1, Ordering::AcqRel);
        inbox.pending.fetch_sub(1, Ordering::AcqRel);
        PENDING.fetch_sub(1, Ordering::AcqRel);
        task(g);
        crate::web::checkpoint(g);
        n += 1;
        if budget_us != 0 && start.elapsed() >= std::time::Duration::from_micros(budget_us as u64) {
            break;
        }
    }
    n
}

// ---------------------------------------------------------------------------
// Advisories — one-shot, on stderr, kept for tests
// ---------------------------------------------------------------------------

/// Above this, a synchronous read of a path-backed Blob says so (A-F2).
pub const SYNC_READ_ADVISORY_BYTES: u64 = 16 << 20;
/// Above this, `stream()` of a path-backed Blob says that v1 reads it in one
/// piece before the first chunk (A-F2).
pub const STREAM_ADVISORY_BYTES: u64 = 64 << 20;

fn advised() -> &'static Mutex<(HashSet<String>, Vec<String>)> {
    static A: OnceLock<Mutex<(HashSet<String>, Vec<String>)>> = OnceLock::new();
    A.get_or_init(|| Mutex::new((HashSet::new(), Vec::new())))
}

/// Says `msg` once per `key`, for the process.
pub(crate) fn advise(key: &str, msg: String) {
    let mut a = advised().lock().unwrap();
    if !a.0.insert(key.to_string()) {
        return;
    }
    eprintln!("[redjs] {msg}");
    a.1.push(msg);
}

/// Every File-layer advisory said so far, in order.
pub fn advisories() -> Vec<String> {
    advised().lock().unwrap().1.clone()
}

pub(crate) fn advise_sync_read(consumer: &str, bytes: u64) {
    advise(
        &format!("A-F2:{consumer}"),
        format!(
            "File (A-F2): a {} MiB File was read synchronously by {consumer} on the UI thread; \
             use file.stream() / a chunked upload (file.slice()) instead.",
            bytes >> 20
        ),
    );
}

/// A synchronous consumer's read failed: the message its error carries, and
/// — once per consumer — why it surfaced HERE. A browser reads a File body
/// when the request is sent and fails the fetch (or the body read) then; this
/// runtime reads it when the body is taken (A-F2), so the same failure comes
/// out of the constructor or the call that took it.
pub(crate) fn sync_read_failed(consumer: &str, e: ReadError) -> String {
    advise(
        &format!("A-F2-fail:{consumer}"),
        format!(
            "File (A-F2): {consumer} read a File synchronously and it failed ({}); in a browser the same \
             failure surfaces later, when the body is actually read.",
            e.name()
        ),
    );
    format!("{}: {}", e.name(), e.message())
}

// ---------------------------------------------------------------------------
// Per-context state: the originals `stream()` reads through
// ---------------------------------------------------------------------------

struct BlobState {
    /// `Blob.prototype.bytes` and `.slice` as installed — `stream()` reads
    /// through these, so a page that patches the prototype later cannot reach
    /// into a stream.
    bytes_fn: u64,
    slice_fn: u64,
    /// `(blob, bytes, slice, size, oneRead) => ReadableStream`, made once the
    /// Streams surface exists ([`install_stream`]).
    stream_factory: u64,
}

fn registry() -> &'static Mutex<HashMap<usize, BlobState>> {
    static R: OnceLock<Mutex<HashMap<usize, BlobState>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The values here were protected in a heap that no longer exists. The
/// context's inbox is retired too: a read that finishes later drops its
/// result instead of parking it (and its bytes) where nothing will run it.
pub(crate) fn forget(gone: crate::context::Gone) {
    registry().lock().unwrap().remove(&gone.key);
    let inbox = inboxes().lock().unwrap().remove(&gone.key);
    if let Some(i) = inbox {
        retire(&i);
    }
}

#[cfg(test)]
pub(crate) fn has_inbox_work(g: *mut c_void) -> bool {
    lookup(g).is_some_and(|i| i.pending.load(Ordering::Acquire) != 0 || i.queued.load(Ordering::Acquire) != 0)
}

// ---------------------------------------------------------------------------
// Constructing: BlobParts and BlobPropertyBag
// ---------------------------------------------------------------------------

/// One converted `BlobPart`.
pub(crate) enum Part {
    Blob(Range),
    Bytes(Vec<u8>),
    Text(String),
}

/// An object in WebIDL's sense (not a primitive that happens to be a cell:
/// strings, symbols and BigInts are cells too).
pub(crate) unsafe fn is_object(g: *mut c_void, v: u64) -> bool {
    is_cell(v) && !matches!(glue::rjs_kind(g, v), 5..=7)
}

/// WebIDL `sequence<BlobPart>`, through the iteration protocol (so a Set or a
/// generator works and an array-like without `@@iterator` does not, as in a
/// browser): each element a Blob (shared, never read here), a BufferSource
/// (copied now — it is mutable) or a USVString. Elements are converted as
/// they come, so no JS value is held where the collector cannot see it.
pub(crate) unsafe fn convert_parts(g: *mut c_void, parts: u64, who: &str) -> Result<Vec<Part>, ()> {
    let st = state(g);
    let mut out = Vec::new();
    if parts == UNDEF {
        return Ok(out);
    }
    let not_a_sequence = || {
        throw_type_error(g, &format!("Failed to construct '{who}': The provided value cannot be converted to a sequence."));
    };
    if !is_object(g, parts) {
        not_a_sequence();
        return Err(());
    }
    let a = glue::rjs_atom_iterator(g);
    let mut method = UNDEF;
    let rc = glue::rjs_get(g, parts, a, &mut method);
    glue::rjs_atom_free(g, a);
    if rc != 0 {
        glue::rjs_throw(g, method);
        return Err(());
    }
    if glue::rjs_kind(g, method) != 9 {
        not_a_sequence();
        return Err(());
    }
    let iter = call(g, method, parts, &[]).map_err(|e| glue::rjs_throw(g, e))?;
    if !is_object(g, iter) {
        throw_type_error(g, "Result of the Symbol.iterator method is not an object");
        return Err(());
    }
    let next = try_get(g, iter, "next")?;
    loop {
        let r = call(g, next, iter, &[]).map_err(|e| glue::rjs_throw(g, e))?;
        if !is_object(g, r) {
            throw_type_error(g, "Iterator result is not an object");
            return Err(());
        }
        if glue::rjs_to_boolean(g, try_get(g, r, "done")?) != 0 {
            break;
        }
        let p = try_get(g, r, "value")?;
        if let Some(b) = host_data::<BlobData>(g, st.blob_class, p) {
            out.push(Part::Blob(b.range()));
        } else if let Some(b) = bufferish(g, p) {
            out.push(Part::Bytes(b.to_vec()));
        } else {
            let s = to_js_string(g, p)?;
            out.push(Part::Text(read_string(g, s).unwrap_or_default()));
        }
    }
    Ok(out)
}

/// `BlobPropertyBag`, members in WebIDL order (`endings`, then `type`):
/// `(type, endings == "native")`. A `type` outside U+0020–U+007E is `""`,
/// otherwise lowercased.
pub(crate) unsafe fn convert_bag(g: *mut c_void, options: u64, who: &str) -> Result<(String, bool), ()> {
    let mut mime = String::new();
    let mut native = false;
    if options == UNDEF || options == NULL {
        return Ok((mime, native));
    }
    if !is_object(g, options) {
        let bag = if who == "File" { "FilePropertyBag" } else { "BlobPropertyBag" };
        throw_type_error(g, &format!("Failed to construct '{who}': The provided value is not of type '{bag}'."));
        return Err(());
    }
    let e = try_get(g, options, "endings")?;
    if e != UNDEF {
        let e = to_js_string(g, e)?;
        let s = read_string(g, e).unwrap_or_default();
        match s.as_str() {
            "transparent" => {}
            "native" => native = true,
            _ => {
                throw_type_error(
                    g,
                    &format!("Failed to construct '{who}': Failed to read the 'endings' property from 'BlobPropertyBag': The provided value '{s}' is not a valid enum value of type EndingType."),
                );
                return Err(());
            }
        }
    }
    let t = try_get(g, options, "type")?;
    if t != UNDEF {
        let Ok(s) = to_js_string(g, t) else { return Err(()) };
        let s = read_string(g, s).unwrap_or_default();
        if s.chars().all(|c| ('\u{20}'..='\u{7e}').contains(&c)) {
            mime = s.to_ascii_lowercase();
        }
    }
    Ok((mime, native))
}

/// File API "convert line endings to native": every CR, LF and CRLF becomes
/// the platform's line break.
fn native_line_endings(s: &str) -> String {
    let nl = if cfg!(windows) { "\r\n" } else { "\n" };
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\r' => {
                if it.peek() == Some(&'\n') {
                    it.next();
                }
                out.push_str(nl);
            }
            '\n' => out.push_str(nl),
            c => out.push(c),
        }
    }
    out
}

/// The parts, joined. One Blob part is SHARED (`new File([file], "renamed")`,
/// `new Blob([file], { type })` stay path-backed and lazy); anything else is
/// assembled in memory, reading path-backed parts now — and a part that can
/// no longer be read makes the whole Blob [`Source::Unreadable`], with the
/// size it would have had.
pub(crate) fn assemble(parts: Vec<Part>, native: bool, consumer: &str) -> Range {
    if parts.len() == 1 && matches!(parts[0], Part::Blob(_)) {
        let Some(Part::Blob(r)) = parts.into_iter().next() else { unreachable!() };
        return r;
    }
    let parts: Vec<Part> = parts
        .into_iter()
        .map(|p| match p {
            Part::Text(s) if native => Part::Text(native_line_endings(&s)),
            p => p,
        })
        .collect();
    let total: u64 = parts
        .iter()
        .map(|p| match p {
            Part::Blob(r) => r.len,
            Part::Bytes(b) => b.len() as u64,
            Part::Text(s) => s.len() as u64,
        })
        .sum();
    let disk: u64 = parts
        .iter()
        .map(|p| match p {
            Part::Blob(r) if matches!(r.source, Source::Disk(_)) => r.len,
            _ => 0,
        })
        .sum();
    if disk > SYNC_READ_ADVISORY_BYTES {
        advise_sync_read(consumer, disk);
    }
    let mut bytes: Vec<u8> = Vec::new();
    if bytes.try_reserve_exact(total as usize).is_err() {
        return Range { source: Source::Unreadable(ReadError::NotReadable), offset: 0, len: total };
    }
    for p in parts {
        match p {
            Part::Blob(r) => match r.in_memory() {
                Some(b) => bytes.extend_from_slice(b),
                None => match r.read() {
                    Ok(b) => bytes.extend_from_slice(&b),
                    // `size` is the sum of the parts whether or not they can
                    // still be read; only the reads fail.
                    Err(e) => return Range { source: Source::Unreadable(e), offset: 0, len: total },
                },
            },
            Part::Bytes(b) => bytes.extend_from_slice(&b),
            Part::Text(s) => bytes.extend_from_slice(s.as_bytes()),
        }
    }
    let len = bytes.len() as u64;
    Range { source: Source::Memory(Arc::new(bytes)), offset: 0, len }
}

// ---------------------------------------------------------------------------
// The class
// ---------------------------------------------------------------------------

unsafe fn this_blob<'a>(g: *mut c_void, cf: *mut c_void) -> Option<&'a mut BlobData> {
    let st = state(g);
    host_data::<BlobData>(g, st.blob_class, cf_this(cf))
}

/// A memory Blob over `bytes` (fetch/Response internals, IndexedDB reads).
pub unsafe fn create(g: *mut c_void, proto: u64, bytes: Vec<u8>, mime: String) -> u64 {
    let st = state(g);
    let proto = if is_cell(proto) { proto } else { st.blob.proto };
    let len = bytes.len() as u64;
    host_create(g, st.blob_class, proto, BlobData { source: Source::Memory(Arc::new(bytes)), offset: 0, len, mime, file: None })
}

#[crate::host_fn]
unsafe fn construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let st = state(g);
    let Ok(parts) = convert_parts(g, cf_arg(cf, 0), "Blob") else { return UNDEF };
    let Ok((mime, native)) = convert_bag(g, cf_arg(cf, 1), "Blob") else { return UNDEF };
    let range = assemble(parts, native, "the Blob constructor");
    let proto = {
        let nt = cf_new_target(cf);
        let p = if is_cell(nt) { get(g, nt, "prototype") } else { UNDEF };
        if is_cell(p) { p } else { st.blob.proto }
    };
    host_create(g, st.blob_class, proto, BlobData { source: range.source, offset: range.offset, len: range.len, mime, file: None })
}

#[crate::host_fn]
unsafe fn get_size(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_blob(g, cf) {
        Some(b) => enc_f64(b.len as f64),
        None => throw_type_error(g, "Blob.prototype.size called on a non-Blob"),
    }
}

#[crate::host_fn]
unsafe fn get_type(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_blob(g, cf) {
        Some(b) => {
            let mime = b.mime.clone();
            js_string(g, &mime)
        }
        None => throw_type_error(g, "Blob.prototype.type called on a non-Blob"),
    }
}

#[derive(Clone, Copy)]
enum As {
    ArrayBuffer,
    Bytes,
    Text,
}

/// The bytes as the JS value a read resolves with; `Err` is the exception.
unsafe fn convert(g: *mut c_void, as_: As, bytes: &[u8]) -> Result<u64, u64> {
    match as_ {
        As::Text => {
            // "UTF-8 decode": a leading BOM goes, bad sequences become U+FFFD
            // (Rust's lossy decode substitutes maximal subparts, as WHATWG's).
            let body = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
            let s = String::from_utf8_lossy(body);
            Ok(js_string(g, &s))
        }
        As::ArrayBuffer | As::Bytes => {
            let u8v = make_u8(g, bytes);
            if !is_cell(u8v) {
                let m: Vec<u16> = "Array buffer allocation failed".encode_utf16().collect();
                let mut err = UNDEF;
                glue::rjs_error_create(g, 2, m.as_ptr(), m.len(), &mut err);
                return Err(err);
            }
            Ok(if matches!(as_, As::Bytes) { u8v } else { get(g, u8v, "buffer") })
        }
    }
}

/// Protected promise capabilities, carried to the read thread and back.
struct Settle {
    resolve: u64,
    reject: u64,
}
unsafe impl Send for Settle {}

impl Settle {
    unsafe fn finish(self, g: *mut c_void, r: Result<u64, u64>) {
        let _ = match r {
            Ok(v) => call(g, self.resolve, UNDEF, &[v]),
            Err(e) => call(g, self.reject, UNDEF, &[e]),
        };
        crate::ffi::redjs_unprotect(g, self.resolve);
        crate::ffi::redjs_unprotect(g, self.reject);
    }
}

/// A promise for `range` read as `as_`: settled now for memory, from the pump
/// for a path-backed range.
unsafe fn read_promise(g: *mut c_void, range: Range, as_: As) -> u64 {
    if let Some(bytes) = range.in_memory() {
        return match convert(g, as_, bytes) {
            Ok(v) => resolved_promise(g, v),
            Err(e) => super::domexception::rejected_with(g, e),
        };
    }
    if let Source::Unreadable(e) = range.source {
        return super::domexception::rejected(g, e.name(), e.message());
    }
    let (mut res, mut rej, mut p) = (0u64, 0u64, 0u64);
    if crate::ffi::redjs_promise_create(g, &mut res, &mut rej, &mut p) != 0 {
        return UNDEF;
    }
    crate::ffi::redjs_protect(g, res);
    crate::ffi::redjs_protect(g, rej);
    let settle = Settle { resolve: res, reject: rej };
    read_async(
        g,
        range,
        Box::new(move |g, r| unsafe {
            let v = match r {
                Ok(bytes) => convert(g, as_, &bytes),
                Err(e) => Err(e.to_js(g)),
            };
            settle.finish(g, v);
        }),
    );
    p
}

unsafe fn read_method(g: *mut c_void, cf: *mut c_void, as_: As, name: &str) -> u64 {
    match this_blob(g, cf) {
        Some(b) => {
            let r = b.range();
            read_promise(g, r, as_)
        }
        None => rejected_promise(g, 1, &format!("Blob.prototype.{name} called on a non-Blob")),
    }
}

#[crate::host_fn]
unsafe fn array_buffer(g: *mut c_void, cf: *mut c_void) -> u64 {
    read_method(g, cf, As::ArrayBuffer, "arrayBuffer")
}

#[crate::host_fn]
unsafe fn bytes_fn(g: *mut c_void, cf: *mut c_void) -> u64 {
    read_method(g, cf, As::Bytes, "bytes")
}

#[crate::host_fn]
unsafe fn text(g: *mut c_void, cf: *mut c_void) -> u64 {
    read_method(g, cf, As::Text, "text")
}

/// `slice(start, end, contentType)`: a window onto the same source — never a
/// read, whichever source it is.
#[crate::host_fn]
unsafe fn slice(g: *mut c_void, cf: *mut c_void) -> u64 {
    let st = state(g);
    let Some(b) = this_blob(g, cf) else {
        return throw_type_error(g, "Blob.prototype.slice called on a non-Blob");
    };
    let size = b.len as f64;
    // `[Clamp] long long`: NaN is 0, the rest is clamped and rounded half to
    // even (WebIDL), then made relative to the size (File API slice steps).
    // The conversions run script (`valueOf`), and what that throws propagates.
    let rel = |v: u64, def: f64| -> Result<f64, ()> {
        if v == UNDEF {
            return Ok(def);
        }
        let d = to_number(g, v)?;
        let d = if d.is_nan() { 0.0 } else { d.clamp(-9.223_372_036_854_776e18, 9.223_372_036_854_776e18).round_ties_even() };
        Ok(if d < 0.0 { (size + d).max(0.0) } else { d.min(size) })
    };
    let Ok(start) = rel(cf_arg(cf, 0), 0.0) else { return UNDEF };
    let Ok(end) = rel(cf_arg(cf, 1), size) else { return UNDEF };
    let span = (end - start).max(0.0) as u64;
    let ct = cf_arg(cf, 2);
    let mime = if ct == UNDEF {
        String::new()
    } else {
        let Ok(ct) = to_js_string(g, ct) else { return UNDEF };
        let s = read_string(g, ct).unwrap_or_default();
        if s.chars().all(|c| ('\u{20}'..='\u{7e}').contains(&c)) { s.to_ascii_lowercase() } else { String::new() }
    };
    let Some(b) = this_blob(g, cf) else {
        return throw_type_error(g, "Blob.prototype.slice called on a non-Blob");
    };
    // A slice of a File is a Blob (no name): the IDL returns `Blob`.
    let data = BlobData { source: b.source.clone(), offset: b.offset + start as u64, len: span, mime, file: None };
    host_create(g, st.blob_class, st.blob.proto, data)
}

/// `stream()`: a `ReadableStream` of `Uint8Array` chunks of at most 64 KiB.
/// A memory Blob is read a chunk at a time; a path-backed one is read once,
/// off the JS thread, and then chunked — the true pull source is the m16
/// "native byte source" seam, phase 2 (A-F2 past 64 MiB says so).
#[crate::host_fn]
unsafe fn stream(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    let Some(b) = this_blob(g, cf) else {
        return throw_type_error(g, "Blob.prototype.stream called on a non-Blob");
    };
    let (len, one_read) = (b.len, !matches!(b.source, Source::Memory(_)));
    if matches!(b.source, Source::Disk(_)) && len > STREAM_ADVISORY_BYTES {
        advise(
            "A-F2:stream",
            format!(
                "File (A-F2): stream() of a {} MiB File reads it in one piece before the first chunk in this \
                 runtime (a browser reads it as you pull). For a file this size, read it in slices instead: \
                 file.slice(i, i + n).arrayBuffer().",
                len >> 20
            ),
        );
    }
    let fns = registry().lock().unwrap().get(&(g as usize)).map(|s| (s.stream_factory, s.bytes_fn, s.slice_fn));
    let Some((factory, bytes, slice)) = fns.filter(|f| is_cell(f.0)) else {
        return throw_type_error(g, "Blob.prototype.stream: this context has no ReadableStream");
    };
    match call(g, factory, UNDEF, &[this, bytes, slice, enc_f64(len as f64), enc_bool(one_read)]) {
        Ok(v) => v,
        Err(e) => {
            glue::rjs_throw(g, e);
            UNDEF
        }
    }
}


pub unsafe fn install(g: *mut c_void) -> (crate::capi::JSClassRef, Class) {
    let cls = make_host_class::<BlobData>("Blob");
    let class = define_class(g, &ClassSpec {
        name: "Blob",
        arity: 0,
        construct: Some(construct),
        parent_proto: 0,
        methods: &[
            ("arrayBuffer", 0, array_buffer),
            ("bytes", 0, bytes_fn),
            ("text", 0, text),
            ("slice", 0, slice),
            ("stream", 0, stream),
        ],
        getters: &[("size", get_size), ("type", get_type)],
        accessors: &[],
    });
    let keep = |v: u64| {
        crate::ffi::redjs_protect(g, v);
        v
    };
    let bytes_fn = keep(get(g, class.proto, "bytes"));
    let slice_fn = keep(get(g, class.proto, "slice"));
    registry().lock().unwrap().insert(g as usize, BlobState { bytes_fn, slice_fn, stream_factory: UNDEF });
    crate::web::register_pump_source(crate::web::PumpSource { inflight: src_inflight, ready: src_ready, drain: src_drain });
    (cls, class)
}

/// The `stream()` factory, made once `ReadableStream` exists (after
/// `streams::install_surface`, before any user script — so a page that later
/// replaces `ReadableStream` cannot change what `stream()` returns).
///
/// `highWaterMark: 0`: nothing is read until a reader asks, as for a
/// browser's blob stream.
pub(crate) unsafe fn install_stream(g: *mut c_void) {
    // Everything the stream touches is captured here, at install: a page that
    // patches `Uint8Array.prototype.slice` or `Reflect.apply` later reaches
    // nothing inside a stream.
    const SRC: &str = "(() => {\n\
        const RS = ReadableStream, apply = Reflect.apply, u8slice = Uint8Array.prototype.slice;\n\
        const CHUNK = 65536;\n\
        return function (blob, bytes, slice, size, oneRead) {\n\
          let at = 0, all = null;\n\
          return new RS({\n\
            async pull(c) {\n\
              if (at >= size) { c.close(); return; }\n\
              const end = at + CHUNK < size ? at + CHUNK : size;\n\
              let part;\n\
              if (oneRead) {\n\
                if (all === null) all = await apply(bytes, blob, []);\n\
                part = apply(u8slice, all, [at, end]);\n\
              } else {\n\
                part = await apply(bytes, apply(slice, blob, [at, end]), []);\n\
              }\n\
              at = end;\n\
              c.enqueue(part);\n\
              if (at >= size) { all = null; c.close(); }\n\
            },\n\
            cancel() { all = null; at = size; },\n\
          }, { highWaterMark: 0 });\n\
        };\n\
      })()";
    let url = "redjs/blob-stream";
    let mut out = 0u64;
    let rc = glue::rjs_eval8(g, SRC.as_ptr(), SRC.len(), url.as_ptr(), url.len(), &mut out);
    if rc != 0 || !is_cell(out) {
        return;
    }
    crate::ffi::redjs_protect(g, out);
    if let Some(s) = registry().lock().unwrap().get_mut(&(g as usize)) {
        s.stream_factory = out;
    }
}

/// The Blob (or File) behind `v`, if it is one.
pub(crate) unsafe fn data_of<'a>(g: *mut c_void, v: u64) -> Option<&'a BlobData> {
    let st = state(g);
    host_data::<BlobData>(g, st.blob_class, v).map(|b| &*b)
}

/// A Blob (or File, when `data.file` is set) over existing data — shared, not
/// copied (the serializer's attachments land here).
pub(crate) unsafe fn from_data(g: *mut c_void, data: BlobData) -> u64 {
    let st = state(g);
    host_create(g, st.blob_class, st.blob.proto, data)
}

/// The bytes of a Blob value, read NOW on this thread, for a consumer that
/// cannot wait (see the module docs). `None` = not a Blob. A path-backed
/// range past 16 MiB says so once per `consumer` (A-F2).
pub unsafe fn bytes_of(g: *mut c_void, v: u64, consumer: &str) -> Option<Result<Vec<u8>, ReadError>> {
    let st = state(g);
    let b = host_data::<BlobData>(g, st.blob_class, v)?;
    let r = b.range();
    if matches!(r.source, Source::Disk(_)) && r.len > SYNC_READ_ADVISORY_BYTES {
        advise_sync_read(consumer, r.len);
    }
    Some(r.read())
}

/// The source and window of a Blob (or File) value, with nothing read: for a
/// host consumer that does the I/O itself, off this thread. The [`Range`] is
/// `Send`; [`Range::copy_into`] and [`Range::read`] apply the snapshot rule
/// to a path-backed one. `None` = not a Blob.
///
/// # Safety
/// `g` must be a live context with the web API installed, on its thread.
pub unsafe fn range_of(g: *mut c_void, v: u64) -> Option<Range> {
    let st = state(g);
    host_data::<BlobData>(g, st.blob_class, v).map(|b| b.range())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_copy_streams_a_file_range_under_the_snapshot_rule() {
        let dir = std::env::temp_dir().join(format!("redjs-copy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("copy.bin");
        // Larger than one 64 KiB piece, so the loop runs more than once.
        let bytes: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&p, &bytes).unwrap();
        let m = std::fs::metadata(&p).unwrap();
        let disk = Arc::new(DiskFile { path: p.clone(), size: m.len(), mtime: m.modified().ok() });
        let r = Range { source: Source::Disk(disk.clone()), offset: 70_000, len: 100_000 };
        let mut out = Vec::new();
        r.copy_into(&mut out).unwrap();
        assert_eq!(out, &bytes[70_000..170_000]);
        let mem = Range { source: Source::Memory(Arc::new(b"hello".to_vec())), offset: 1, len: 3 };
        let mut out = Vec::new();
        mem.copy_into(&mut out).unwrap();
        assert_eq!(out, b"ell");
        // The file changed since the snapshot: the copy refuses, as a read does.
        std::fs::write(&p, b"changed").unwrap();
        assert!(matches!(r.copy_into(&mut Vec::new()), Err(CopyError::Read(ReadError::NotReadable))));
        std::fs::remove_file(&p).unwrap();
        assert!(matches!(r.copy_into(&mut Vec::new()), Err(CopyError::Read(ReadError::NotFound))));
        let _ = std::fs::remove_dir(&dir);
        // A destination that refuses is the caller's own error, untouched.
        struct Full;
        impl std::io::Write for Full {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::from_raw_os_error(112))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        match mem.copy_into(&mut Full) {
            Err(CopyError::Write(e)) => assert_eq!(e.raw_os_error(), Some(112)),
            other => panic!("expected the write error, got {other:?}"),
        }
    }

    /// More panicking jobs than there are read threads, then an ordinary one:
    /// it still runs. Before, each panic took its thread's slot for good, and
    /// after eight every read in the process waited forever.
    #[test]
    fn a_job_that_panics_does_not_take_its_thread_slot_with_it() {
        for _ in 0..MAX_READS_IN_FLIGHT * 2 {
            submit(Box::new(|| panic!("a read job panics on purpose (test)")));
        }
        let (tx, rx) = std::sync::mpsc::channel();
        submit(Box::new(move || {
            let _ = tx.send(());
        }));
        rx.recv_timeout(std::time::Duration::from_secs(20)).expect("the pool still runs jobs after panics");
    }

    #[test]
    fn native_endings_normalise_every_line_break() {
        let nl = if cfg!(windows) { "\r\n" } else { "\n" };
        assert_eq!(native_line_endings("a\nb\r\nc\rd"), format!("a{nl}b{nl}c{nl}d"));
        assert_eq!(native_line_endings("none"), "none");
    }

    #[test]
    fn a_memory_range_reads_its_window_only() {
        let r = Range { source: Source::Memory(Arc::new(b"hello".to_vec())), offset: 1, len: 3 };
        assert_eq!(r.in_memory(), Some(&b"ell"[..]));
        assert_eq!(r.read().unwrap(), b"ell");
        assert!(r.fits());
        let bad = Range { source: Source::Memory(Arc::new(b"hi".to_vec())), offset: 1, len: 5 };
        assert!(!bad.fits());
        assert_eq!(bad.read(), Err(ReadError::NotReadable));
    }

    #[test]
    fn a_failed_part_makes_an_unreadable_blob_of_the_full_size() {
        let gone = Range {
            source: Source::Disk(Arc::new(DiskFile { path: std::env::temp_dir().join("redjs-no-such-file-ever"), size: 4, mtime: None })),
            offset: 0,
            len: 4,
        };
        let r = assemble(vec![Part::Text("ab".into()), Part::Blob(gone)], false, "test");
        assert_eq!(r.len, 6);
        assert_eq!(r.read(), Err(ReadError::NotFound));
    }

    #[test]
    fn one_blob_part_is_shared_and_many_are_joined() {
        let r = Range { source: Source::Memory(Arc::new(b"abc".to_vec())), offset: 0, len: 3 };
        let shared = assemble(vec![Part::Blob(r.clone())], false, "test");
        assert!(matches!((&shared.source, &r.source), (Source::Memory(a), Source::Memory(b)) if Arc::ptr_eq(a, b)));
        let joined = assemble(vec![Part::Blob(r), Part::Text("d".into()), Part::Bytes(vec![b'e'])], false, "test");
        assert_eq!(joined.read().unwrap(), b"abcde");
    }
}

#[cfg(test)]
mod jsc_tests {
    use crate::webapi::file::testing::*;
    use crate::webapi::testing::*;

    /// `range_of` hands a host the window a Blob value names: a slice of a
    /// picked File is the file's range, a script Blob is its memory, and
    /// anything else is `None`.
    #[test]
    fn range_of_names_the_window_without_reading() {
        let c = file_ctx();
        let t = TempFile::new("range.bin", b"0123456789");
        t.expose(&c, "picked");
        s(&c, "globalThis.part = picked.slice(2, 6); globalThis.mem = new Blob(['abc']); 0");
        let g = c.raw();
        let global = unsafe { crate::glue::rjs_global(g) };
        let get = |name: &str| unsafe { crate::webapi::util::get(g, global, name) };
        let part = unsafe { super::range_of(g, get("part")) }.expect("a slice is a Blob");
        assert!(matches!(&part.source, super::Source::Disk(f) if f.path == t.0));
        assert_eq!((part.offset, part.len), (2, 4));
        let mut out = Vec::new();
        part.copy_into(&mut out).unwrap();
        assert_eq!(out, b"2345");
        let mem = unsafe { super::range_of(g, get("mem")) }.unwrap();
        assert_eq!(mem.in_memory(), Some(&b"abc"[..]));
        s(&c, "globalThis.notBlob = {}; 0");
        assert!(unsafe { super::range_of(g, get("notBlob")) }.is_none());
    }

    /// A read still running when its context goes away: its result is dropped
    /// (and its buffer freed) when it lands. It used to re-create the dead
    /// context's scheduler entry and park there forever. m25 review, finding 5.
    #[test]
    fn a_read_that_finishes_after_teardown_is_dropped_not_parked() {
        use std::sync::{mpsc, Arc, Mutex};
        let c = file_ctx();
        let t = TempFile::new("late.bin", &[1u8; 1 << 16]);
        t.expose(&c, "f");
        let g = c.raw();
        // Occupy every read thread, so the read below finishes after the teardown.
        let (release, gate) = mpsc::channel::<()>();
        let gate = Arc::new(Mutex::new(gate));
        for _ in 0..super::MAX_READS_IN_FLIGHT {
            let gate = Arc::clone(&gate);
            super::submit(Box::new(move || {
                let _ = gate.lock().unwrap().recv_timeout(std::time::Duration::from_secs(10));
            }));
        }
        s(&c, "f.arrayBuffer().then(() => { globalThis.late = 1; }); 0");
        assert!(super::has_inbox_work(g), "the read is outstanding");
        // The teardown, as the context registry runs it for a recycled address.
        let before = crate::context::id_of(g).unwrap();
        let after = unsafe { crate::context::register(g) };
        assert_ne!(before, after);
        assert!(!crate::web::has_scheduler(g));
        for _ in 0..super::MAX_READS_IN_FLIGHT {
            let _ = release.send(());
        }
        // Wait for the pool to have run the read: a job queued after it is
        // taken no sooner, and then the read itself has had time to land.
        let (tx, rx) = mpsc::channel();
        super::submit(Box::new(move || {
            let _ = tx.send(());
        }));
        rx.recv_timeout(std::time::Duration::from_secs(20)).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(!crate::web::has_scheduler(g), "nothing re-created the dead context's queue");
        assert!(!super::has_inbox_work(g), "nothing is waiting for the new context either");
    }

    #[test]
    fn parts_convert_as_a_webidl_sequence() {
        let c = file_ctx();
        assert_eq!(run(&c, "return await new Blob(new Set(['a', 'b'])).text();"), "ab");
        assert_eq!(run(&c, "return await new Blob((function* () { yield 'x'; yield new Uint8Array([121]); })()).text();"), "xy");
        assert_eq!(run(&c, "return await new Blob([1, null, { toString() { return 'o'; } }]).text();"), "1nullo");
        for bad in ["new Blob('abc')", "new Blob({ length: 1, 0: 'a' })", "new Blob(5)"] {
            assert!(throws(&c, bad).contains("cannot be converted to a sequence"), "{bad}");
        }
        assert_eq!(throws(&c, "new Blob([{ toString() { throw new Error('mine'); } }])"), "mine");
        assert!(throws(&c, "new Blob([], 'text/plain')").contains("BlobPropertyBag"));
    }

    #[test]
    fn the_bag_reads_endings_then_type() {
        let c = file_ctx();
        let nl = if cfg!(windows) { "13,10" } else { "10" };
        assert_eq!(run(&c, "return [...await new Blob(['\\n'], { endings: 'native' }).bytes()].join();"), nl);
        assert_eq!(run(&c, "return [...await new Blob(['\\r\\n'], { endings: 'transparent' }).bytes()].join();"), "13,10");
        assert!(throws(&c, "new Blob([], { endings: 'unix' })").contains("EndingType"));
        ok(&c, "(() => { const order = []; new Blob([], { get type() { order.push('type'); return 'A/B'; }, get endings() { order.push('endings'); } }); \
                 return order.join() === 'endings,type'; })()");
        ok(&c, "new Blob([], { type: 'A/B' }).type === 'a/b' && new Blob([], { type: 'a\\u00e9' }).type === ''");
    }

    #[test]
    fn slice_clamps_rounds_and_converts_like_webidl() {
        let c = file_ctx();
        assert_eq!(run(&c, "return await new Blob(['0123456789']).slice(1.5, 4.5).text();"), "23", "[Clamp] rounds half to even");
        assert_eq!(run(&c, "return await new Blob(['0123456789']).slice(-3).text();"), "789");
        assert_eq!(run(&c, "return await new Blob(['0123456789']).slice(NaN, Infinity).text();"), "0123456789");
        assert_eq!(run(&c, "return await new Blob(['0123456789']).slice(8, 2).text();"), "");
        ok(&c, "Blob.prototype.slice.length === 0 && new Blob(['x']).slice(0, 1, 5).type === '5'");
        assert_eq!(throws(&c, "new Blob(['x']).slice({ valueOf() { throw new Error('vo'); } })"), "vo");
        ok(&c, "Object.prototype.toString.call(new Blob([])) === '[object Blob]'");
    }

    #[test]
    fn reads_of_a_memory_blob_settle_without_the_pump() {
        // No scheduler in this context: a memory read must not need one.
        let c = ctx();
        s(&c, "globalThis.got = null; new Blob(['abc']).text().then(t => { got = t; }); 0");
        c.drain_microtasks();
        ok(&c, "got === 'abc'");
        assert!(throws(&c, "Blob.prototype.stream.call({})").contains("non-Blob"));
    }
}
