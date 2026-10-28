//! WPT IndexedDB under redjs (m25 indexeddb-design §8.2).
//!
//! `harness = false`: `main` is the runner. Each test file runs in its OWN
//! child process (this same binary, `REDJS_WPT_CHILD=<file>`): a fresh JSC
//! context, a fresh IndexedDB server and memory backend, and nothing a
//! previous file left open — the isolation a browser gets from a new page.
//! (JSC contexts cannot be released in this prebuilt, so in-process reuse
//! would also grow without bound over ~200 files.)
//!
//! - `cargo test --release --test wpt_idb` — the smoke tier.
//! - `cargo test --release --test wpt_idb -- --ignored` (or `REDJS_WPT=gate`)
//!   — the gate tier: every vendored file.
//! - `REDJS_WPT_ONLY=<substring>` — only matching files, either tier.
//! - `REDJS_WPT_REPORT=<path>` — a per-file summary written there.
//!
//! In the child: `document` is deleted before testharness loads (so it picks
//! `ShellTestEnvironment`), `location` names the file, the `// META: script=`
//! lines are expanded, and the context is pumped with the browser-exact loop
//! (a microtask checkpoint after every task) under the API lock until the
//! completion callback fires. The shell environment has no harness timeout,
//! so the runner enforces 10 s per file (60 s for `timeout=long`) by calling
//! testharness's own `timeout()`.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use redjs::api::{Context, Error};
use redjs::ffi;

/// The per-change gate (design §8.2): ~25 files, < 15 s.
const SMOKE: &[&str] = &[
    "key_valid",
    "key_invalid",
    "keyorder",
    "idbfactory_cmp",
    "idbkeyrange",
    "keypath",
    "keygenerator",
    "idbobjectstore_put",
    "idbobjectstore_add",
    "idbobjectstore_getAll",
    "idbindex-multientry",
    "idbcursor-continuePrimaryKey",
    "idbcursor-direction-index",
    "transaction-deactivation-timing",
    "event-dispatch-active-flag",
    "fire-success-event-exception",
    "fire-error-event-exception",
    "request_bubble-and-capture",
    "transaction-lifetime",
    "transaction-abort-request-error",
    "upgrade-transaction-lifecycle-committed",
    "idbversionchangeevent",
    "idbfactory_open",
    "get-databases",
    "idb-explicit-commit",
    "value",
    "structured-clone",
];

/// PUMP_TIMERS | PUMP_RAF | PUMP_MICROTASKS | PUMP_CHECKPOINT_PER_TASK.
const FLAGS: u32 = 1 | 2 | 4 | 8;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("wpt")
}

fn main() {
    if let Ok(file) = std::env::var("REDJS_WPT_CHILD") {
        child(&file);
        return;
    }
    let args: Vec<String> = std::env::args().collect();
    // `cargo test` probes harness=false targets with `--list`.
    if args.iter().any(|a| a == "--list") {
        println!("wpt_idb: test");
        return;
    }
    let gate = args.iter().any(|a| a == "--ignored" || a == "--include-ignored")
        || std::env::var("REDJS_WPT").map(|v| v == "gate").unwrap_or(false);
    std::process::exit(parent(gate));
}

// ---------------------------------------------------------------------------
// Expectations
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Expect {
    skip: BTreeSet<String>,
    fail: BTreeSet<(String, String)>,
    harness: BTreeSet<String>,
}

fn expectations() -> Expect {
    let text = std::fs::read_to_string(root().join("expectations.txt")).unwrap_or_default();
    let mut e = Expect::default();
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        if let Some(f) = l.strip_prefix("skip ") {
            e.skip.insert(f.trim().to_string());
        } else if let Some(f) = l.strip_prefix("harness ") {
            e.harness.insert(f.trim().to_string());
        } else if let Some(rest) = l.strip_prefix("fail ") {
            if let Some((f, name)) = rest.split_once(" :: ") {
                e.fail.insert((f.trim().to_string(), name.to_string()));
            }
        }
    }
    e
}

// ---------------------------------------------------------------------------
// Parent: select, run each file in a child, compare
// ---------------------------------------------------------------------------

struct FileOutcome {
    harness: (i32, String),
    tests: Vec<(i32, String, String)>,
    crashed: Option<String>,
    secs: f64,
}

fn limit_for(file: &str) -> Duration {
    let src = std::fs::read_to_string(root().join("IndexedDB").join(file)).unwrap_or_default();
    if src.lines().any(|l| l.trim() == "// META: timeout=long") {
        Duration::from_secs(60)
    } else {
        Duration::from_secs(10)
    }
}

fn run_child(file: &str) -> FileOutcome {
    let start = Instant::now();
    let exe = std::env::current_exe().expect("current exe");
    let mut child = Command::new(exe)
        .env("REDJS_WPT_CHILD", file)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the WPT child");
    let hard = limit_for(file) + Duration::from_secs(20);
    let mut out = String::new();
    let mut err = String::new();
    let mut so = child.stdout.take().unwrap();
    let mut se = child.stderr.take().unwrap();
    let t_out = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = so.read_to_string(&mut s);
        s
    });
    let t_err = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = se.read_to_string(&mut s);
        s
    });
    let status = loop {
        if let Some(st) = child.try_wait().expect("wait") {
            break Some(st);
        }
        if start.elapsed() > hard {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    out.push_str(&t_out.join().unwrap_or_default());
    err.push_str(&t_err.join().unwrap_or_default());
    let mut o = FileOutcome { harness: (-1, String::new()), tests: Vec::new(), crashed: None, secs: start.elapsed().as_secs_f64() };
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("@@HARNESS ") {
            let mut p = rest.splitn(2, '\t');
            let st = p.next().unwrap_or("-1").parse().unwrap_or(-1);
            o.harness = (st, p.next().unwrap_or("").to_string());
        } else if let Some(rest) = line.strip_prefix("@@TEST ") {
            let mut p = rest.splitn(3, '\t');
            let st = p.next().unwrap_or("-1").parse().unwrap_or(-1);
            let name = p.next().unwrap_or("").to_string();
            let msg = p.next().unwrap_or("").to_string();
            o.tests.push((st, name, msg));
        }
    }
    match status {
        None => o.crashed = Some("killed: the child outlived its hard limit".into()),
        Some(st) if !st.success() => {
            let tail: String = err.lines().rev().take(8).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join(" | ");
            o.crashed = Some(format!("child exited with {st}: {tail}"));
        }
        _ => {}
    }
    o
}

const STATUS: [&str; 5] = ["PASS", "FAIL", "TIMEOUT", "NOTRUN", "PRECONDITION_FAILED"];

fn parent(gate: bool) -> i32 {
    let dir = root().join("IndexedDB");
    let only = std::env::var("REDJS_WPT_ONLY").ok();
    let mut files: Vec<String> = if gate {
        let mut v: Vec<String> = std::fs::read_dir(&dir)
            .expect("tests/wpt/IndexedDB")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".any.js"))
            .collect();
        v.push("crashtests/create-index.any.js".into());
        v
    } else {
        SMOKE.iter().map(|s| format!("{s}.any.js")).collect()
    };
    files.sort();
    if let Some(o) = &only {
        files.retain(|f| f.contains(o.as_str()));
    }
    let exp = expectations();
    let tier = if gate { "gate" } else { "smoke" };
    let started = Instant::now();
    let mut problems: Vec<String> = Vec::new();
    let mut report = String::new();
    let (mut total, mut passed, mut files_run) = (0usize, 0usize, 0usize);
    let mut per_status: BTreeMap<&str, usize> = BTreeMap::new();
    for f in &files {
        if exp.skip.contains(f) {
            report.push_str(&format!("{f}\tSKIP\n"));
            continue;
        }
        files_run += 1;
        let o = run_child(f);
        let p = o.tests.iter().filter(|t| t.0 == 0).count();
        total += o.tests.len();
        passed += p;
        for t in &o.tests {
            *per_status.entry(STATUS.get(t.0 as usize).copied().unwrap_or("?")).or_default() += 1;
        }
        report.push_str(&format!("{f}\t{p}/{}\t{:.1}s{}\n", o.tests.len(), o.secs, if o.harness.0 != 0 { format!("\tharness {} {}", o.harness.0, o.harness.1) } else { String::new() }));
        if let Some(c) = &o.crashed {
            problems.push(format!("{f}: {c}"));
        }
        if o.harness.0 != 0 && !exp.harness.contains(f) {
            problems.push(format!("{f}: harness status {} ({})", o.harness.0, o.harness.1));
        }
        if o.harness.0 == 0 && exp.harness.contains(f) {
            problems.push(format!("{f}: harness now OK — remove its `harness` line"));
        }
        let seen: BTreeSet<&str> = o.tests.iter().map(|t| t.1.as_str()).collect();
        for (st, name, msg) in &o.tests {
            let expected_fail = exp.fail.contains(&(f.clone(), name.clone()));
            if *st == 0 && expected_fail {
                problems.push(format!("{f} :: {name}: UNEXPECTED PASS"));
            } else if *st != 0 && !expected_fail {
                let s = STATUS.get(*st as usize).copied().unwrap_or("?");
                problems.push(format!("{f} :: {name}: {s} — {msg}"));
            }
        }
        for (ef, en) in &exp.fail {
            if ef == f && !seen.contains(en.as_str()) && o.crashed.is_none() {
                eprintln!("[wpt] note: stale expectation (no such subtest ran): {ef} :: {en}");
            }
        }
        eprintln!("[wpt] {f}: {p}/{} ({:.1}s)", o.tests.len(), o.secs);
    }
    let summary = format!(
        "WPT IndexedDB {tier}: {files_run} files, {passed}/{total} subtests pass ({:.1}%), {:.1}s; {per_status:?}",
        if total > 0 { 100.0 * passed as f64 / total as f64 } else { 0.0 },
        started.elapsed().as_secs_f64()
    );
    eprintln!("{summary}");
    report.push_str(&summary);
    report.push('\n');
    if let Ok(p) = std::env::var("REDJS_WPT_REPORT") {
        let mut r = report.clone();
        if !problems.is_empty() {
            r.push_str("\nUnexpected:\n");
            for p in &problems {
                r.push_str(p);
                r.push('\n');
            }
        }
        let _ = std::fs::write(p, r);
    }
    if problems.is_empty() {
        eprintln!("[wpt] every result matches expectations.txt");
        0
    } else {
        eprintln!("[wpt] {} unexpected result(s):", problems.len());
        for p in &problems {
            eprintln!("  {p}");
        }
        1
    }
}

// ---------------------------------------------------------------------------
// Child: one file
// ---------------------------------------------------------------------------

fn eval(ctx: &Context, src: &str, url: &str) -> Result<String, String> {
    match ctx.eval_named(src, url) {
        Ok(v) => Ok(ctx.to_rust_string(v).unwrap_or_default()),
        Err(Error::Exception(e)) => Err(ctx.error_info(e).0),
        Err(e) => Err(format!("{e}")),
    }
}

fn clean(s: &str) -> String {
    s.replace(['\t', '\n', '\r'], " ")
}

fn scripts(src: &str) -> Vec<String> {
    src.lines()
        .filter_map(|l| l.trim().strip_prefix("// META: script="))
        .map(|s| s.trim().to_string())
        .collect()
}

fn resolve(dir: &Path, script: &str) -> PathBuf {
    match script.strip_prefix('/') {
        Some(abs) => root().join(abs),
        None => dir.join(script),
    }
}

fn child(file: &str) {
    let dir = root().join("IndexedDB");
    let path = dir.join(file);
    let file_dir = path.parent().unwrap().to_path_buf();
    let src = std::fs::read_to_string(&path).expect("read the test file");
    let limit = limit_for(file);
    let ctx = Context::new();
    let g = ctx.raw();
    // Timers / rAF / performance first: the web API install expects them.
    unsafe {
        ffi::redjs_web_install(g);
        ffi::redjs_webshim_install(g, core::ptr::null(), 0, core::ptr::null(), 0);
    }

    let href = format!("app://wpt/IndexedDB/{file}");
    let prelude = format!(
        "delete globalThis.document;\n\
         if (typeof self === 'undefined') globalThis.self = globalThis;\n\
         Object.defineProperty(globalThis, 'location', {{ configurable: true, writable: true, value: {{\n\
           href: {href:?}, origin: 'app://wpt', protocol: 'app:', host: 'wpt', hostname: 'wpt', port: '',\n\
           pathname: {path:?}, search: '', hash: '', toString() {{ return this.href; }} }} }});\n\
         'ok'",
        path = format!("/IndexedDB/{file}")
    );
    let fatal = |what: &str, e: String| {
        println!("@@HARNESS 1\t{}", clean(&format!("{what}: {e}")));
        std::process::exit(0);
    };
    if let Err(e) = eval(&ctx, &prelude, "wpt/prelude.js") {
        fatal("prelude", e);
    }
    let th = std::fs::read_to_string(root().join("resources").join("testharness.js")).expect("testharness.js");
    if let Err(e) = eval(&ctx, &th, "/resources/testharness.js") {
        fatal("testharness.js", e);
    }
    let hook = "add_completion_callback(function (tests, status) {\n\
                  var clean = function (s) { return String(s == null ? '' : s).replace(/[\\t\\n\\r]/g, ' '); };\n\
                  globalThis.__wpt = '@@HARNESS ' + status.status + '\\t' + clean(status.message) + '\\n' +\n\
                    tests.map(function (t) { return '@@TEST ' + t.status + '\\t' + clean(t.name) + '\\t' + clean(t.message); }).join('\\n');\n\
                }); 'ok'";
    if let Err(e) = eval(&ctx, hook, "wpt/hook.js") {
        fatal("hook", e);
    }
    for s in scripts(&src) {
        let p = resolve(&file_dir, &s);
        let code = match std::fs::read_to_string(&p) {
            Ok(c) => c,
            Err(e) => fatal("META script", format!("{}: {e}", p.display())),
        };
        if let Err(e) = eval(&ctx, &code, &s) {
            fatal(&format!("META script {s}"), e);
        }
    }
    // An exception at the top level of the test file is a harness error in a
    // browser too; testharness would record it through `error` — report it.
    let top = eval(&ctx, &src, file);

    let start = Instant::now();
    let mut forced: Option<Instant> = None;
    unsafe { ffi::redjs_lock(g) };
    let out = loop {
        unsafe { ffi::redjs_pump(g, FLAGS, core::ptr::null_mut()) };
        let r = eval(&ctx, "typeof globalThis.__wpt === 'string' ? globalThis.__wpt : ''", "wpt/poll.js").unwrap_or_default();
        if !r.is_empty() {
            break Some(r);
        }
        match forced {
            None if start.elapsed() > limit => {
                let _ = eval(&ctx, "typeof timeout === 'function' && timeout(); 'ok'", "wpt/timeout.js");
                forced = Some(Instant::now());
            }
            Some(t) if t.elapsed() > Duration::from_secs(3) => break None,
            _ => {}
        }
        std::thread::sleep(Duration::from_micros(200));
    };
    unsafe { ffi::redjs_unlock(g) };
    match out {
        Some(r) => {
            let mut stdout = std::io::stdout();
            let mut text = r;
            if let Err(e) = &top {
                // Keep the harness verdict but say why the file stopped early.
                text = text.replacen("@@HARNESS 0\t", &format!("@@HARNESS 1\t{} ", clean(&format!("top-level throw: {e}"))), 1);
            }
            let _ = writeln!(stdout, "{text}");
            let _ = stdout.flush();
        }
        None => {
            println!("@@HARNESS 2\t{}", clean(&format!("no completion after the {}s limit{}", limit.as_secs(), match &top {
                Err(e) => format!(" (top-level throw: {e})"),
                Ok(_) => String::new(),
            })));
        }
    }
    // Do not tear down: this prebuilt cannot release a context, and the
    // process is about to exit anyway.
    let _ = std::io::stdout().flush();
    std::process::exit(0);
}
