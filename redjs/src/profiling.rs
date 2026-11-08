//! Opt-in, bounded main-thread spans for embedders. No JSC values are retained.
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, VecDeque},
    marker::PhantomData,
    panic::Location,
    rc::Rc,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

static ENABLED: AtomicBool = AtomicBool::new(false);
static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);
static NEXT_EVENT: AtomicU64 = AtomicU64::new(1);
const MAX_TEXT: usize = 192;
const MAX_URL: usize = 768;
const MAX_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct Limits {
    pub frames: usize,
    pub spans_per_frame: usize,
    pub total_spans: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            frames: 1200,
            spans_per_frame: 2048,
            total_spans: 50_000,
        }
    }
}
#[derive(Clone, Debug)]
pub struct Callsite {
    pub kind: &'static str,
    pub function: String,
    pub url: String,
    pub line: u32,
    pub column: u32,
}
#[derive(Clone, Debug)]
pub struct EventSpan {
    pub id: u64,
    pub parent_id: Option<u64>,
    pub category: &'static str,
    pub name: String,
    pub start_ms: f64,
    pub duration_ms: f64,
    pub self_ms: f64,
    pub depth: usize,
    pub callsite: Option<Callsite>,
    pub counters: BTreeMap<String, u64>,
}
#[derive(Clone, Debug)]
pub struct Category {
    pub category: &'static str,
    pub self_ms: f64,
    pub total_ms: f64,
    pub count: usize,
}
#[derive(Clone, Debug)]
pub struct Hint {
    pub code: &'static str,
    pub severity: &'static str,
    pub message: String,
    pub count: u64,
    pub span_id: Option<u64>,
}
#[derive(Clone, Debug)]
pub struct FrameSummary {
    pub id: u64,
    pub start_ms: f64,
    pub duration_ms: f64,
    pub cpu_ms: f64,
    pub span_count: usize,
    pub dropped_spans: u64,
    pub categories: Vec<Category>,
    pub hints: Vec<Hint>,
}
#[derive(Clone, Debug)]
pub struct Status {
    pub recording: bool,
    pub session_id: u64,
    pub start_epoch_ms: f64,
    pub limits: Limits,
    pub frame_count: usize,
    pub dropped_frames: u64,
    pub dropped_spans: u64,
}
#[derive(Clone, Debug)]
pub struct FramePage {
    pub status: Status,
    pub total: usize,
    pub offset: usize,
    pub next_offset: Option<usize>,
    pub frames: Vec<FrameSummary>,
}
#[derive(Clone, Debug)]
pub struct DetailPage {
    pub session_id: u64,
    pub frame: FrameSummary,
    pub offset: usize,
    pub total: usize,
    pub next_offset: Option<usize>,
    pub truncated: bool,
    pub spans: Vec<EventSpan>,
}

struct Frame {
    summary: FrameSummary,
    spans: Vec<EventSpan>,
    bytes: usize,
}
struct OpenSpan {
    id: u64,
    index: usize,
    started: Instant,
    children: Duration,
}
struct CurrentFrame {
    id: u64,
    started: Instant,
    spans: Vec<EventSpan>,
    stack: Vec<OpenSpan>,
    dropped: u64,
    counters: BTreeMap<String, u64>,
    last_layout_write_epoch: Option<u64>,
}
struct Recorder {
    id: u64,
    recording: bool,
    started: Instant,
    epoch: f64,
    target_context: Option<usize>,
    target_window: Option<u64>,
    limits: Limits,
    frames: VecDeque<Frame>,
    current: Option<CurrentFrame>,
    next_frame: u64,
    stored_spans: usize,
    stored_bytes: usize,
    dropped_frames: u64,
    dropped_spans: u64,
}
thread_local! {
    static RECORDER: RefCell<Option<Recorder>> = const { RefCell::new(None) };
    static CONTEXT: Cell<usize> = const { Cell::new(0) };
    static WINDOW: Cell<u64> = const { Cell::new(0) };
}
#[inline]
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}
fn text(value: &str, maximum: usize) -> String {
    let mut end = value.len().min(maximum);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}
fn relative(start: Instant, value: Instant) -> f64 {
    value.saturating_duration_since(start).as_secs_f64() * 1000.
}
fn allowed(recorder: &Recorder) -> bool {
    recorder.recording
        && recorder.target_context.is_none_or(|target| {
            CONTEXT.with(|context| context.get() == 0 || context.get() == target)
        })
        && recorder
            .target_window
            .is_none_or(|target| WINDOW.with(|window| window.get() == 0 || window.get() == target))
}

pub fn start(mut limits: Limits, target_context: Option<usize>) -> Status {
    limits.frames = limits.frames.clamp(16, 10_000);
    limits.spans_per_frame = limits.spans_per_frame.clamp(64, 8192);
    limits.total_spans = limits.total_spans.clamp(limits.spans_per_frame, 100_000);
    let started = Instant::now();
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
        * 1000.;
    let recorder = Recorder {
        id: NEXT_SESSION.fetch_add(1, Ordering::Relaxed),
        recording: true,
        started,
        epoch,
        target_context,
        target_window: None,
        limits,
        frames: VecDeque::new(),
        current: None,
        next_frame: 1,
        stored_spans: 0,
        stored_bytes: 0,
        dropped_frames: 0,
        dropped_spans: 0,
    };
    let status = recorder.status();
    RECORDER.with(|cell| *cell.borrow_mut() = Some(recorder));
    ENABLED.store(true, Ordering::Relaxed);
    frame_start();
    status
}
pub fn stop() -> Option<Status> {
    let result = RECORDER.with(|cell| {
        let mut cell = cell.borrow_mut();
        let recorder = cell.as_mut()?;
        recorder.close_frame(Instant::now());
        recorder.recording = false;
        Some(recorder.status())
    });
    ENABLED.store(false, Ordering::Relaxed);
    result
}
pub fn clear() {
    ENABLED.store(false, Ordering::Relaxed);
    RECORDER.with(|cell| {
        cell.borrow_mut().take();
    });
}
pub fn status() -> Option<Status> {
    RECORDER.with(|cell| cell.borrow().as_ref().map(Recorder::status))
}
pub fn frame_start() {
    if !enabled() {
        return;
    }
    RECORDER.with(|cell| {
        let mut cell = cell.borrow_mut();
        let Some(recorder) = cell.as_mut().filter(|recorder| allowed(recorder)) else {
            return;
        };
        let now = Instant::now();
        recorder.close_frame(now);
        let id = recorder.next_frame;
        recorder.next_frame += 1;
        recorder.current = Some(CurrentFrame {
            id,
            started: now,
            spans: Vec::new(),
            stack: Vec::new(),
            dropped: 0,
            counters: BTreeMap::new(),
            last_layout_write_epoch: None,
        });
    });
}
pub fn frames(offset: usize, limit: usize) -> Option<FramePage> {
    RECORDER.with(|cell| {
        let cell = cell.borrow();
        let recorder = cell.as_ref()?;
        let total = recorder.frames.len();
        let offset = offset.min(total);
        let limit = limit.clamp(1, 240);
        let frames: Vec<_> = recorder
            .frames
            .iter()
            .skip(offset)
            .take(limit)
            .map(|frame| frame.summary.clone())
            .collect();
        let next = offset + frames.len();
        Some(FramePage {
            status: recorder.status(),
            total,
            offset,
            next_offset: (next < total).then_some(next),
            frames,
        })
    })
}
pub fn frame_detail(id: u64, offset: usize, limit: usize) -> Option<DetailPage> {
    RECORDER.with(|cell| {
        let cell = cell.borrow();
        let recorder = cell.as_ref()?;
        let frame = recorder
            .frames
            .iter()
            .find(|frame| frame.summary.id == id)?;
        let total = frame.spans.len();
        let offset = offset.min(total);
        let limit = limit.clamp(1, 1000);
        let end = offset.saturating_add(limit).min(total);
        Some(DetailPage {
            session_id: recorder.id,
            frame: frame.summary.clone(),
            offset,
            total,
            next_offset: (end < total).then_some(end),
            truncated: frame.summary.dropped_spans != 0,
            spans: frame.spans[offset..end].to_vec(),
        })
    })
}

pub fn set_window_target(window: u64) {
    RECORDER.with(|cell| {
        if let Some(recorder) = cell.borrow_mut().as_mut() {
            recorder.target_window = (window != 0).then_some(window);
        }
    });
}
pub struct WindowScope {
    previous: Option<u64>,
    _thread: PhantomData<Rc<()>>,
}
impl WindowScope {
    pub fn enter(window: u64) -> Self {
        Self {
            previous: enabled().then(|| WINDOW.with(|current| current.replace(window))),
            _thread: PhantomData,
        }
    }
}
impl Drop for WindowScope {
    fn drop(&mut self) {
        if let Some(previous) = self.previous {
            WINDOW.with(|window| window.set(previous));
        }
    }
}

pub struct ContextScope {
    previous: Option<usize>,
    _thread: PhantomData<Rc<()>>,
}
impl ContextScope {
    pub fn enter(context: usize) -> Self {
        Self {
            previous: enabled().then(|| CONTEXT.with(|current| current.replace(context))),
            _thread: PhantomData,
        }
    }
}
impl Drop for ContextScope {
    fn drop(&mut self) {
        if let Some(previous) = self.previous {
            CONTEXT.with(|context| context.set(previous));
        }
    }
}
struct JsLock(*mut core::ffi::c_void);
impl Drop for JsLock {
    fn drop(&mut self) {
        let _checkpoint = span("microtask", "JSC API release checkpoint");
        unsafe {
            crate::glue::rjs_unlock(self.0);
        }
    }
}

pub struct Span {
    session: u64,
    id: u64,
    _js_lock: Option<JsLock>,
    _context: Option<ContextScope>,
    _thread: PhantomData<Rc<()>>,
}
impl Span {
    fn empty() -> Self {
        Self {
            session: 0,
            id: 0,
            _js_lock: None,
            _context: None,
            _thread: PhantomData,
        }
    }
    pub fn id(&self) -> u64 {
        self.id
    }
    pub fn counter(&self, name: &str, amount: u64) {
        if self.id != 0 {
            counter(name, amount);
        }
    }
}
impl Drop for Span {
    fn drop(&mut self) {
        if self.id != 0 {
            finish(self.session, self.id);
        }
    }
}
fn current_target_active() -> bool {
    RECORDER.with(|cell| {
        let Ok(mut cell) = cell.try_borrow_mut() else {
            return false;
        };
        let Some(recorder) = cell.as_mut().filter(|recorder| allowed(recorder)) else {
            return false;
        };
        let Some(frame) = recorder.current.as_mut() else {
            return false;
        };
        if frame.spans.len() >= recorder.limits.spans_per_frame || frame.stack.len() >= 128 {
            frame.dropped += 1;
            false
        } else {
            true
        }
    })
}

#[track_caller]
#[inline]
pub fn span(category: &'static str, name: &'static str) -> Span {
    if !enabled() || !current_target_active() {
        return Span::empty();
    }
    let location = Location::caller();
    begin(
        category,
        name,
        Some(Callsite {
            kind: "rust",
            function: name.into(),
            url: text(location.file(), MAX_URL),
            line: location.line(),
            column: location.column(),
        }),
    )
}
#[track_caller]
pub fn host_span(context: usize, name: &'static str) -> Span {
    if !enabled() {
        return Span::empty();
    }
    let scope = ContextScope::enter(context);
    let mut result = span("native", name);
    result._context = Some(scope);
    result
}
pub fn host_span_at(
    context: usize,
    name: &'static str,
    file: &'static str,
    line: u32,
    column: u32,
) -> Span {
    if !enabled() {
        return Span::empty();
    }
    let scope = ContextScope::enter(context);
    if !current_target_active() {
        let mut result = Span::empty();
        result._context = Some(scope);
        return result;
    }
    let mut result = begin(
        "native",
        name,
        Some(Callsite {
            kind: "rust",
            function: name.into(),
            url: text(file, MAX_URL),
            line,
            column,
        }),
    );
    result._context = Some(scope);
    result
}

/// Balanced span callbacks for a renderer that must remain independent of redjs.
pub fn external_begin(
    category: &'static str,
    name: &'static str,
    file: &'static str,
    line: u32,
    column: u32,
) -> u64 {
    if !enabled() || !current_target_active() {
        return 0;
    }
    let mut span = begin(
        category,
        name,
        Some(Callsite {
            kind: "rust",
            function: name.into(),
            url: text(file, MAX_URL),
            line,
            column,
        }),
    );
    let id = span.id;
    span.id = 0;
    id
}
pub fn external_end(id: u64) {
    if id == 0 || !enabled() {
        return;
    }
    let session = RECORDER.with(|cell| cell.borrow().as_ref().map(|recorder| recorder.id));
    if let Some(session) = session {
        finish(session, id);
    }
}

/// Begins a span for an original function invoked by a JS-side wrapper.
/// No value, guard or VM lock is retained after this function returns.
/// # Safety
/// Must be called FROM executing JavaScript on this context's VM thread. That
/// invocation owns the VM lock for both this metadata read and the callback.
/// A nonzero returned token must be balanced with `external_callback_end`.
pub unsafe fn external_callback_begin(global: *mut core::ffi::c_void, function: u64) -> u64 {
    if !enabled() {
        return 0;
    }
    let _context = ContextScope::enter(global as usize);
    if !current_target_active() {
        return 0;
    }
    let callsite = function_callsite(global, function);
    let name = callsite
        .as_ref()
        .map(|site| site.function.as_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("JavaScript callback")
        .to_owned();
    let mut scope = begin("js", &name, callsite);
    let token = scope.id;
    scope.id = 0;
    token
}
pub fn external_callback_end(token: u64) {
    external_end(token);
}

fn begin(category: &'static str, name: &str, callsite: Option<Callsite>) -> Span {
    if !enabled() {
        return Span::empty();
    }
    RECORDER.with(|cell| {
        let Ok(mut cell) = cell.try_borrow_mut() else {
            return Span::empty();
        };
        let Some(recorder) = cell.as_mut().filter(|recorder| allowed(recorder)) else {
            return Span::empty();
        };
        let Some(frame) = recorder.current.as_mut() else {
            return Span::empty();
        };
        if frame.spans.len() >= recorder.limits.spans_per_frame || frame.stack.len() >= 128 {
            frame.dropped += 1;
            return Span::empty();
        }
        let now = Instant::now();
        let id = NEXT_EVENT.fetch_add(1, Ordering::Relaxed);
        let parent_id = frame.stack.last().map(|parent| parent.id);
        let depth = frame.stack.len();
        let index = frame.spans.len();
        frame.spans.push(EventSpan {
            id,
            parent_id,
            category,
            name: text(name, MAX_TEXT),
            start_ms: relative(recorder.started, now),
            duration_ms: 0.,
            self_ms: 0.,
            depth,
            callsite,
            counters: BTreeMap::new(),
        });
        frame.stack.push(OpenSpan {
            id,
            index,
            started: now,
            children: Duration::ZERO,
        });
        Span {
            session: recorder.id,
            id,
            _js_lock: None,
            _context: None,
            _thread: PhantomData,
        }
    })
}
fn finish(session: u64, id: u64) {
    if !enabled() {
        return;
    }
    RECORDER.with(|cell| {
        let Ok(mut cell) = cell.try_borrow_mut() else {
            return;
        };
        let Some(recorder) = cell
            .as_mut()
            .filter(|recorder| recorder.id == session && recorder.recording)
        else {
            return;
        };
        let Some(frame) = recorder.current.as_mut() else {
            return;
        };
        if let Some(index) = frame.stack.iter().position(|span| span.id == id) {
            let now = Instant::now();
            while frame.stack.len() > index {
                frame.finish_top(now);
            }
        }
    });
}
/// An already measured phase. Intended for existing aggregate timing hooks.
pub fn interval(category: &'static str, name: &'static str, started: Instant, ended: Instant) {
    if !enabled() {
        return;
    }
    RECORDER.with(|cell| {
        let Ok(mut cell) = cell.try_borrow_mut() else {
            return;
        };
        let Some(recorder) = cell.as_mut().filter(|recorder| allowed(recorder)) else {
            return;
        };
        let Some(frame) = recorder.current.as_mut() else {
            return;
        };
        if frame.spans.len() >= recorder.limits.spans_per_frame {
            frame.dropped += 1;
            return;
        }
        let started = started.max(frame.started);
        let duration = ended.saturating_duration_since(started);
        let parent = frame
            .stack
            .last_mut()
            .filter(|parent| parent.started <= started);
        let (parent_id, depth) = match parent {
            Some(parent) => {
                parent.children += duration;
                (Some(parent.id), frame.stack.len())
            }
            None => (None, 0),
        };
        let id = NEXT_EVENT.fetch_add(1, Ordering::Relaxed);
        frame.spans.push(EventSpan {
            id,
            parent_id,
            category,
            name: text(name, MAX_TEXT),
            start_ms: relative(recorder.started, started),
            duration_ms: duration.as_secs_f64() * 1000.,
            self_ms: duration.as_secs_f64() * 1000.,
            depth,
            callsite: None,
            counters: BTreeMap::new(),
        });
    });
}
pub fn measured(category: &'static str, name: &'static str, milliseconds: f32) {
    if !enabled() {
        return;
    }
    let now = Instant::now();
    let duration = Duration::from_secs_f64(f64::from(milliseconds.max(0.)) / 1000.);
    interval(
        category,
        name,
        now.checked_sub(duration).unwrap_or(now),
        now,
    );
}
pub fn counter(name: &str, amount: u64) {
    if !enabled() {
        return;
    }
    RECORDER.with(|cell| {
        let Ok(mut cell) = cell.try_borrow_mut() else {
            return;
        };
        let Some(recorder) = cell.as_mut().filter(|recorder| allowed(recorder)) else {
            return;
        };
        let Some(frame) = recorder.current.as_mut() else {
            return;
        };
        if name == "forcedLayouts" && amount > 0 {
            let writes = frame.counters.get("domWrites").copied().unwrap_or(0);
            if frame
                .last_layout_write_epoch
                .is_some_and(|previous| writes > previous)
            {
                *frame
                    .counters
                    .entry("readWritePingPongs".into())
                    .or_default() += amount;
            }
            frame.last_layout_write_epoch = Some(writes);
        }
        let name = text(name, 64);
        if frame.counters.len() < 64 || frame.counters.contains_key(&name) {
            *frame.counters.entry(name.clone()).or_default() += amount;
        }
        if let Some(parent) = frame.stack.last() {
            if let Some(span) = frame.spans.get_mut(parent.index) {
                if span.counters.len() < 32 || span.counters.contains_key(&name) {
                    *span.counters.entry(name).or_default() += amount;
                }
            }
        }
    });
}
pub fn mark(label: &str) {
    if !enabled() {
        return;
    }
    let span = begin("user", label, None);
    drop(span);
}
impl CurrentFrame {
    fn finish_top(&mut self, now: Instant) {
        let Some(open) = self.stack.pop() else { return };
        let duration = now.saturating_duration_since(open.started);
        if let Some(span) = self.spans.get_mut(open.index) {
            span.duration_ms = duration.as_secs_f64() * 1000.;
            span.self_ms = duration.saturating_sub(open.children).as_secs_f64() * 1000.;
        }
        if let Some(parent) = self.stack.last_mut() {
            parent.children += duration;
        }
    }
}
impl Recorder {
    fn status(&self) -> Status {
        Status {
            recording: self.recording,
            session_id: self.id,
            start_epoch_ms: self.epoch,
            limits: self.limits.clone(),
            frame_count: self.frames.len(),
            dropped_frames: self.dropped_frames,
            dropped_spans: self.dropped_spans
                + self.current.as_ref().map_or(0, |frame| frame.dropped),
        }
    }
    fn close_frame(&mut self, now: Instant) {
        let Some(mut frame) = self.current.take() else {
            return;
        };
        while !frame.stack.is_empty() {
            frame.finish_top(now);
        }
        let mut categories: BTreeMap<&'static str, Category> = BTreeMap::new();
        let mut cpu_ms = 0.;
        let mut hints = Vec::new();
        for span in &frame.spans {
            let category = categories.entry(span.category).or_insert(Category {
                category: span.category,
                self_ms: 0.,
                total_ms: 0.,
                count: 0,
            });
            category.self_ms += span.self_ms;
            category.total_ms += span.duration_ms;
            category.count += 1;
            if span.category != "wait" && !span.counters.contains_key("wallTime") {
                cpu_ms += span.self_ms;
            }
            if span.category == "js" && span.duration_ms > 8. && hints.len() < 8 {
                hints.push(Hint {
                    code: "long-callback",
                    severity: "warning",
                    message: format!(
                        "{} occupied the main thread for {:.2} ms",
                        span.name, span.duration_ms
                    ),
                    count: 1,
                    span_id: Some(span.id),
                });
            }
        }
        let forced = frame.counters.get("forcedLayouts").copied().unwrap_or(0);
        if forced > 0 {
            hints.push(Hint {
                code: "forced-layout",
                severity: "warning",
                message: "Geometry reads forced synchronous layout; batch DOM writes before reads"
                    .into(),
                count: forced,
                span_id: frame
                    .spans
                    .iter()
                    .find(|span| span.name.starts_with("Forced layout"))
                    .map(|span| span.id),
            });
        }
        let ping_pongs = frame
            .counters
            .get("readWritePingPongs")
            .copied()
            .unwrap_or(0);
        if ping_pongs > 0 {
            hints.push(Hint { code: "read-write-ping-pong", severity: "warning", message: "DOM writes occurred after a forced layout and triggered another synchronous layout; batch those writes before geometry reads".into(), count: ping_pongs, span_id: None });
        }
        let created = frame.counters.get("dom.create").copied().unwrap_or(0);
        let removed = frame.counters.get("dom.remove").copied().unwrap_or(0);
        if created > 128 && removed > 128 {
            hints.push(Hint { code: "dom-node-churn", severity: "warning", message: "Many DOM nodes were created and removed in the same frame; inspect list keys and component remounts".into(), count: created + removed, span_id: None });
        }
        let allocated = frame.counters.get("allocatedBytes").copied().unwrap_or(0);
        let freed = frame.counters.get("freedBytes").copied().unwrap_or(0);
        if allocated > 4 * 1024 * 1024 && freed > allocated / 2 {
            hints.push(Hint {
                code: "allocation-churn",
                severity: "warning",
                message:
                    "Recorded allocation counters show substantial short-lived allocation traffic"
                        .into(),
                count: allocated,
                span_id: None,
            });
        }
        let bytes = frame
            .spans
            .iter()
            .map(|span| {
                std::mem::size_of::<EventSpan>()
                    + span.name.capacity()
                    + span
                        .callsite
                        .as_ref()
                        .map_or(0, |site| site.function.capacity() + site.url.capacity())
                    + span.counters.len() * 96
            })
            .sum::<usize>();
        let summary = FrameSummary {
            id: frame.id,
            start_ms: relative(self.started, frame.started),
            duration_ms: now.saturating_duration_since(frame.started).as_secs_f64() * 1000.,
            cpu_ms,
            span_count: frame.spans.len(),
            dropped_spans: frame.dropped,
            categories: categories.into_values().collect(),
            hints,
        };
        self.dropped_spans += frame.dropped;
        self.stored_spans += frame.spans.len();
        self.stored_bytes += bytes;
        self.frames.push_back(Frame {
            summary,
            spans: frame.spans,
            bytes,
        });
        while self.frames.len() > self.limits.frames
            || self.stored_spans > self.limits.total_spans
            || self.stored_bytes > MAX_BYTES
        {
            if let Some(frame) = self.frames.pop_front() {
                self.stored_spans -= frame.spans.len();
                self.stored_bytes -= frame.bytes;
                self.dropped_frames += 1;
            } else {
                break;
            }
        }
    }
}

#[repr(C)]
struct FunctionInfo {
    line: u32,
    column: u32,
    name_len: u32,
    url_len: u32,
    name: [u8; 192],
    url: [u8; 768],
}
extern "C" {
    fn rjs_profile_function_info(
        global: *mut core::ffi::c_void,
        function: u64,
        info: *mut FunctionInfo,
    ) -> i32;
}
unsafe fn function_callsite(global: *mut core::ffi::c_void, function: u64) -> Option<Callsite> {
    let mut info = FunctionInfo {
        line: 0,
        column: 0,
        name_len: 0,
        url_len: 0,
        name: [0; 192],
        url: [0; 768],
    };
    let callsite = if rjs_profile_function_info(global, function, &mut info) != 0 {
        Some(Callsite {
            kind: "js",
            function: String::from_utf8_lossy(
                &info.name[..(info.name_len as usize).min(info.name.len())],
            )
            .into_owned(),
            url: String::from_utf8_lossy(&info.url[..(info.url_len as usize).min(info.url.len())])
                .into_owned(),
            line: info.line,
            column: info.column,
        })
    } else {
        None
    };
    callsite
}

/// Reads executable metadata, never `function.name` or another JS property.
/// # Safety
/// The function value must belong to this live context on its VM thread.
pub unsafe fn js_span(global: *mut core::ffi::c_void, function: u64, name: &'static str) -> Span {
    js_span_inner(global, function, name, false)
}
unsafe fn js_span_inner(
    global: *mut core::ffi::c_void,
    function: u64,
    name: &'static str,
    intrinsic_name: bool,
) -> Span {
    if !enabled() {
        return Span::empty();
    }
    let scope = ContextScope::enter(global as usize);
    let accept = current_target_active();
    if !accept {
        let mut result = Span::empty();
        result._context = Some(scope);
        return result;
    }
    crate::glue::rjs_lock(global);
    let lock = JsLock(global);
    let callsite = function_callsite(global, function);
    let label = intrinsic_name.then(|| {
        callsite
            .as_ref()
            .map(|site| site.function.as_str())
            .filter(|name| !name.is_empty())
            .unwrap_or(name)
            .to_owned()
    });
    let mut result = begin("js", label.as_deref().unwrap_or(name), callsite);
    result._context = Some(scope);
    result._js_lock = Some(lock);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    #[test]
    fn nested_spans_have_stable_parents_and_bounded_pages() {
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        start(Limits::default(), None);
        {
            let outer = span("js", "outer");
            {
                let inner = span("native", "inner");
                assert_ne!(outer.id(), inner.id());
                counter("domWrites", 1);
            }
        }
        frame_start();
        stop();
        let page = frames(0, 1).expect("recording");
        let id = page.frames[0].id;
        let first = frame_detail(id, 0, 1).expect("frame");
        let second = frame_detail(id, 1, 1).expect("second span");
        assert_eq!(second.spans[0].parent_id, Some(first.spans[0].id));
        assert_eq!(first.next_offset, Some(1));
        assert_eq!(first.total, 2);
        assert_eq!(first.spans[0].counters.get("domWrites"), None);
        assert_eq!(second.spans[0].counters.get("domWrites"), Some(&1));
        clear();
    }
    #[test]
    fn old_guards_cannot_close_a_restarted_recording() {
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        start(Limits::default(), None);
        let old = span("js", "old");
        start(Limits::default(), None);
        let current = span("js", "current");
        drop(old);
        drop(current);
        frame_start();
        stop();
        let page = frames(0, 1).expect("recording");
        let detail = frame_detail(page.frames[0].id, 0, 10).expect("frame");
        assert_eq!(detail.spans.len(), 1);
        assert_eq!(detail.spans[0].name, "current");
        clear();
    }
    #[test]
    fn overflow_and_stopped_recordings_remain_queryable() {
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        start(
            Limits {
                frames: 16,
                spans_per_frame: 64,
                total_spans: 64,
            },
            None,
        );
        for _ in 0..100 {
            drop(span("native", "bounded"));
        }
        frame_start();
        let stopped = stop().expect("recording");
        assert!(!stopped.recording);
        assert!(stopped.dropped_spans >= 36);
        let page = frames(0, 240).expect("recording");
        assert!(page.total <= 16);
        let detail = frame_detail(page.frames[0].id, 0, 1000).expect("frame");
        assert_eq!(detail.total, 64);
        assert!(detail.truncated);
        clear();
        assert!(!enabled());
        assert!(status().is_none());
    }
    #[test]
    fn executable_metadata_does_not_read_user_function_name_getters() {
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        let context = crate::api::Context::new();
        context.eval_named("globalThis.getterCalls=0; function profileTarget(){return 42}; Object.defineProperty(profileTarget,'name',{get(){getterCalls++;throw new Error('observable getter')}});", "vn://profile-location.js").expect("script");
        let function = context.global_get("profileTarget").expect("function");
        start(Limits::default(), Some(context.raw() as usize));
        let value = context
            .call(function, crate::api::Value::undefined(), &[])
            .expect("callback");
        assert_eq!(value.as_number(), Some(42.));
        frame_start();
        stop();
        let page = frames(0, 1).expect("recording");
        let detail = frame_detail(page.frames[0].id, 0, 100).expect("frame");
        let site = detail
            .spans
            .iter()
            .find_map(|span| span.callsite.as_ref().filter(|site| site.kind == "js"))
            .expect("JS executable metadata");
        assert_eq!(site.function, "profileTarget");
        assert_eq!(site.url, "vn://profile-location.js");
        assert!(site.line > 0);
        assert_eq!(
            context.eval("getterCalls").expect("counter").as_number(),
            Some(0.)
        );
        clear();
    }
    fn install_original_callback_controls(context: &crate::api::Context) {
        use crate::api::{HostArgs, Value};
        let begin = context
            .function("__profileBegin", 1, |global, frame| unsafe {
                Value::number(external_callback_begin(global, HostArgs::new(frame).arg(0).0) as f64)
                    .0
            })
            .expect("begin control");
        let end = context
            .function("__profileEnd", 1, |_global, frame| unsafe {
                external_callback_end(HostArgs::new(frame).arg(0).as_number().unwrap_or(0.) as u64);
                Value::undefined().0
            })
            .expect("end control");
        context
            .global_set("__profileBegin", begin)
            .expect("begin global");
        context.global_set("__profileEnd", end).expect("end global");
    }
    #[test]
    fn timer_registered_before_recording_attributes_the_original_function() {
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        let context = crate::api::Context::new();
        assert_eq!(unsafe { crate::web::install(context.raw()) }, 0);
        install_original_callback_controls(&context);
        context.eval_named("globalThis.ran=0; function profiledTimer(){ const until=performance.now()+12; while(performance.now()<until){}; ran++; }", "vn://devtools/app/main.ts").expect("user callback");
        context.eval_named("((cb)=>{setTimeout(function(){const token=__profileBegin(cb);if(!token)return cb.apply(this,arguments);try{return cb.apply(this,arguments)}finally{__profileEnd(token)}},0)})(profiledTimer)", "vn://vendor.js").expect("register wrapped timer before recording");
        start(Limits::default(), Some(context.raw() as usize));
        unsafe {
            crate::glue::rjs_lock(context.raw());
            crate::web::pump(
                context.raw(),
                crate::web::PUMP_TIMERS | crate::web::PUMP_MICROTASKS,
                std::ptr::null_mut(),
            );
            crate::glue::rjs_unlock(context.raw());
        }
        stop();
        let page = frames(0, 240).expect("recording");
        let frame = page
            .frames
            .iter()
            .find(|frame| frame.hints.iter().any(|hint| hint.code == "long-callback"))
            .expect("actual long callback hint");
        let detail = frame_detail(frame.id, 0, 1000).expect("frame details");
        let original = detail
            .spans
            .iter()
            .find(|span| {
                span.callsite
                    .as_ref()
                    .is_some_and(|site| site.function == "profiledTimer")
            })
            .expect("original callback metadata");
        assert_eq!(original.name, "profiledTimer");
        assert_eq!(
            original.callsite.as_ref().expect("callsite").url,
            "vn://devtools/app/main.ts"
        );
        assert!(
            frame
                .hints
                .iter()
                .any(|hint| hint.code == "long-callback" && hint.span_id == Some(original.id)),
            "hint must point at the actual user callback"
        );
        assert_eq!(context.eval("ran").expect("result").as_number(), Some(1.));
        clear();
    }
    #[test]
    fn recording_stopped_inside_original_callback_leaves_no_external_lifetime() {
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        let context = crate::api::Context::new();
        install_original_callback_controls(&context);
        let finish = context
            .function("__stopProfile", 0, |_global, _frame| {
                stop();
                crate::api::Value::undefined().0
            })
            .expect("stop callback");
        context
            .global_set("__stopProfile", finish)
            .expect("stop global");
        context.eval_named("function originalStop(){__stopProfile();return 42};globalThis.wrapper=function(){const token=__profileBegin(originalStop);try{return originalStop()}finally{__profileEnd(token)}}", "vn://stop-callback.js").expect("callback");
        let wrapper = context.global_get("wrapper").expect("wrapper");
        start(Limits::default(), Some(context.raw() as usize));
        assert_eq!(
            context
                .call(wrapper, crate::api::Value::undefined(), &[])
                .expect("call")
                .as_number(),
            Some(42.)
        );
        assert!(!enabled());
        assert_eq!(
            context
                .eval("40+3")
                .expect("subsequent VM call")
                .as_number(),
            Some(43.)
        );
        clear();
    }
}
