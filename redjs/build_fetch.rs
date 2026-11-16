//! Finds the native inputs `pins.txt` names: an override, a repository checkout's `setup.ps1` /
//! `setup.sh` output, or a shared cache that this fills on first use from the pinned URLs, held
//! to the same sha256 checks as the setup scripts.

use std::env;
use std::fs::{self, File};
use std::io::{self, BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::pin;

type Pins = [(String, String)];

/// Frontend table sources the Unix archives omit, taken from the Windows archive of the same engine.
const TABLE_FILES: [&str; 6] = [
    "AOTBuiltinTableAttachments.cpp",
    "AOTBuiltinTableBuiltins.cpp",
    "AOTBuiltinTableNames.cpp",
    "AOTBuiltinTableOverloads.cpp",
    "AOTBuiltinTableTypes.cpp",
    "AOTWire.cpp",
];
/// ABI headers both archives must agree on before those sources are used.
const TABLE_HEADERS: [&str; 4] = [
    "AOTBuiltinTableArrays.h",
    "AOTBuiltinNameSlots.h",
    "AOTBuiltinTableEnums.h",
    "AOTSchemaInlineBuiltins.h",
];

pub fn webkit(manifest: &Path, platform: &str, windows: bool, pins: &Pins) -> PathBuf {
    println!("cargo:rerun-if-env-changed=BUN_WEBKIT_DIR");
    if let Some(dir) = env::var_os("BUN_WEBKIT_DIR").filter(|v| !v.is_empty()) {
        return dir.into();
    }
    let name = if windows {
        "bun-webkit".to_owned()
    } else {
        format!("bun-webkit-{platform}")
    };
    let checkout = manifest.parent().unwrap().join("prebuilt").join(name);
    if webkit_ready(&checkout, pins, windows) {
        return checkout;
    }
    if checkout.join("include/cmakeconfig.h").is_file() {
        println!(
            "cargo:warning=redjs: {} is not the pinned engine; using the redjs cache. setup.ps1 / setup.sh refreshes it.",
            checkout.display()
        );
    }
    let version = pin(pins, "webkit.version");
    let (asset, sha256) = if windows {
        (pin(pins, "webkit.asset"), pin(pins, "webkit.sha256"))
    } else {
        (
            pin(pins, &format!("webkit.{platform}.asset")),
            pin(pins, &format!("webkit.{platform}.sha256")),
        )
    };
    let url = format!(
        "https://github.com/oven-sh/WebKit/releases/download/{}/{asset}",
        pin(pins, "webkit.tag")
    );
    cached(
        &format!("webkit-{}-{platform}", &version[..12]),
        &format!("the JavaScriptCore prebuilt {asset}"),
        "or point BUN_WEBKIT_DIR at an extraction of it",
        |dir| webkit_ready(dir, pins, windows),
        |staging| {
            let archive = staging.join("archive.tar.gz");
            download(&[url.as_str()], &archive, Some(sha256))?;
            let engine = staging.join("engine");
            extract_tar(&archive, &engine, "bun-webkit", |parts| parts[0] != "bin")?;
            fs::remove_file(&archive).map_err(|e| e.to_string())?;
            if !windows {
                add_table_sources(staging, &engine, pins)?;
            }
            if !webkit_ready(&engine, pins, windows) {
                return Err(format!(
                    "{asset} is not engine {version} with webkit.aot = {}",
                    pin(pins, "webkit.aot")
                ));
            }
            Ok(engine)
        },
    )
}

/// The Unix archives lack the frontend table sources; the Windows archive of the same engine has
/// them, and is used only after its generated ABI headers match this platform's.
fn add_table_sources(staging: &Path, engine: &Path, pins: &Pins) -> Result<(), String> {
    let archive = staging.join("tables.tar.gz");
    let url = format!(
        "https://github.com/oven-sh/WebKit/releases/download/{}/{}",
        pin(pins, "webkit.tag"),
        pin(pins, "webkit.asset")
    );
    download(&[url.as_str()], &archive, Some(pin(pins, "webkit.sha256")))?;
    let tables = staging.join("tables");
    extract_tar(&archive, &tables, "bun-webkit", |parts| {
        parts[0] == "include"
    })?;
    fs::remove_file(&archive).map_err(|e| e.to_string())?;
    let jsc = |root: &Path| root.join("include").join("JavaScriptCore");
    if !configuration(&tables).contains(&format!(
        "#define BUN_WEBKIT_VERSION \"{}\"",
        pin(pins, "webkit.version")
    )) {
        return Err("builtin table engine version mismatch".into());
    }
    for name in TABLE_HEADERS {
        let read = |root: &Path| {
            fs::read_to_string(jsc(root).join(name))
                .map(|s| s.replace("\r\n", "\n"))
                .map_err(|e| format!("builtin table header {name}: {e}"))
        };
        if read(&tables)? != read(engine)? {
            return Err(format!("builtin table ABI mismatch: {name}"));
        }
    }
    let generated = jsc(&tables).join("aot");
    if !TABLE_FILES
        .iter()
        .all(|name| generated.join(name).is_file())
    {
        return Err("incomplete builtin table archive".into());
    }
    let target = jsc(engine).join("aot");
    if target.exists() {
        fs::remove_dir_all(&target).map_err(|e| e.to_string())?;
    }
    fs::rename(&generated, &target).map_err(|e| e.to_string())?;
    fs::remove_dir_all(&tables).map_err(|e| e.to_string())
}

fn webkit_ready(dir: &Path, pins: &Pins, windows: bool) -> bool {
    let config = configuration(dir);
    let library = if windows {
        "JavaScriptCore.lib"
    } else {
        "libJavaScriptCore.a"
    };
    config.contains(&format!(
        "#define BUN_WEBKIT_VERSION \"{}\"",
        pin(pins, "webkit.version")
    )) && config
        .lines()
        .any(|l| l.trim() == format!("#define BUN_WEBKIT_ENABLE_AOT {}", pin(pins, "webkit.aot")))
        && dir.join("lib").join(library).is_file()
        && (windows
            || TABLE_FILES
                .iter()
                .all(|f| dir.join("include/JavaScriptCore/aot").join(f).is_file()))
}

pub fn mimalloc(manifest: &Path, pins: &Pins) -> PathBuf {
    let commit = pin(pins, "mimalloc.commit");
    let ready = |dir: &Path| stamp(dir) == commit && dir.join("src/static.c").is_file();
    let checkout = manifest.join("cpp").join("mimalloc");
    if ready(&checkout) {
        return checkout;
    }
    let urls = [
        format!("https://codeload.github.com/oven-sh/mimalloc/tar.gz/{commit}"),
        format!("https://github.com/oven-sh/mimalloc/archive/{commit}.tar.gz"),
        format!("https://api.github.com/repos/oven-sh/mimalloc/tarball/{commit}"),
    ];
    cached(
        &format!("mimalloc-{}", &commit[..12]),
        &format!("oven-sh/mimalloc@{commit}"),
        "",
        ready,
        |staging| {
            let archive = staging.join("archive.tar.gz");
            download(
                &urls.iter().map(String::as_str).collect::<Vec<_>>(),
                &archive,
                None,
            )?;
            let tree = staging.join("mimalloc");
            extract_tar(&archive, &tree, "", |parts| {
                matches!(parts[0].as_str(), "src" | "include" | "LICENSE")
            })?;
            verify_tree(&tree, pin(pins, "mimalloc.tree-sha256"), "mimalloc")?;
            fs::write(tree.join(".redjs-pin"), commit).map_err(|e| e.to_string())?;
            Ok(tree)
        },
    )
}

pub fn sqlite3mc(manifest: &Path, pins: &Pins) -> PathBuf {
    let version = pin(pins, "sqlite3mc.version");
    let tree_sha256 = pin(pins, "sqlite3mc.tree-sha256");
    let receipt = format!("{version} {tree_sha256}");
    let ready =
        |dir: &Path| stamp(dir) == receipt && dir.join("sqlite3mc_amalgamation.c").is_file();
    let checkout = manifest
        .parent()
        .unwrap()
        .join("prebuilt")
        .join("sqlite3mc");
    if ready(&checkout) {
        return checkout;
    }
    let asset = pin(pins, "sqlite3mc.asset");
    let url = format!(
        "https://github.com/utelle/SQLite3MultipleCiphers/releases/download/v{version}/{asset}"
    );
    cached(
        &format!("sqlite3mc-{version}-{}", &tree_sha256[..8]),
        &format!("the SQLite3 Multiple Ciphers amalgamation {asset}"),
        "",
        ready,
        |staging| {
            let archive = staging.join("archive.zip");
            download(
                &[url.as_str()],
                &archive,
                Some(pin(pins, "sqlite3mc.sha256")),
            )?;
            let tree = staging.join("sqlite3mc");
            extract_zip(&archive, &tree)?;
            verify_tree(&tree, tree_sha256, "sqlite3mc")?;
            fs::write(tree.join(".redjs-pin"), &receipt).map_err(|e| e.to_string())?;
            Ok(tree)
        },
    )
}

/// `REDJS_CACHE_DIR`, else `redjs` under the user's cache directory.
fn cache_root() -> PathBuf {
    println!("cargo:rerun-if-env-changed=REDJS_CACHE_DIR");
    let var = |name: &str| {
        env::var_os(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    if let Some(dir) = var("REDJS_CACHE_DIR") {
        return dir;
    }
    let base = if cfg!(windows) {
        var("LOCALAPPDATA")
    } else if cfg!(target_os = "macos") {
        var("HOME").map(|home| home.join("Library/Caches"))
    } else {
        var("XDG_CACHE_HOME").or_else(|| var("HOME").map(|home| home.join(".cache")))
    };
    base.unwrap_or_else(|| PathBuf::from(env::var("OUT_DIR").unwrap()))
        .join("redjs")
}

/// `<cache>/<name>` once `ready`, filling it under a lock so concurrent builds fetch it once.
/// `fill` builds the tree inside a staging directory and returns it; it is moved into place only
/// whole and verified.
fn cached(
    name: &str,
    what: &str,
    alternative: &str,
    ready: impl Fn(&Path) -> bool,
    fill: impl FnOnce(&Path) -> Result<PathBuf, String>,
) -> PathBuf {
    let root = cache_root();
    let dir = root.join(name);
    if ready(&dir) {
        return dir;
    }
    match fill_cache(&root, name, &ready, fill) {
        Ok(true) => println!("cargo:warning=redjs: fetched {what} into {}", dir.display()),
        Ok(false) => {}
        Err(why) => {
            let alternative = if alternative.is_empty() {
                String::new()
            } else {
                format!(", {alternative}")
            };
            panic!(
                "\n\nredjs needs {what} and could not fetch it: {why}\n\
                 Build once with network access to fill the cache at {}{alternative}.\n\
                 REDJS_CACHE_DIR moves the cache; REDJS_PROXY (or HTTPS_PROXY) sets an HTTP proxy.\n",
                root.display()
            )
        }
    }
    dir
}

/// Whether it fetched: another build may have filled the entry while this one waited.
fn fill_cache(
    root: &Path,
    name: &str,
    ready: impl Fn(&Path) -> bool,
    fill: impl FnOnce(&Path) -> Result<PathBuf, String>,
) -> Result<bool, String> {
    if env::var("CARGO_NET_OFFLINE").is_ok_and(|v| !matches!(v.as_str(), "" | "0" | "false")) {
        return Err("CARGO_NET_OFFLINE is set".into());
    }
    let dir = root.join(name);
    fs::create_dir_all(root).map_err(|e| format!("cannot create {}: {e}", root.display()))?;
    let lock = File::create(root.join(format!(".{name}.lock")))
        .and_then(|file| file.lock().map(|()| file))
        .map_err(|e| format!("cannot lock the cache: {e}"))?;
    if ready(&dir) {
        return Ok(false);
    }
    let prefix = format!(".{name}.staging-");
    for entry in fs::read_dir(root).into_iter().flatten().flatten() {
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
    let staging = root.join(format!("{prefix}{}", std::process::id()));
    let result = (|| {
        fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
        let tree = fill(&staging)?;
        if dir.exists() {
            fs::remove_dir_all(&dir)
                .map_err(|e| format!("cannot replace {}: {e}", dir.display()))?;
        }
        fs::rename(&tree, &dir).map_err(|e| format!("cannot move into {}: {e}", dir.display()))
    })();
    let _ = fs::remove_dir_all(&staging);
    drop(lock);
    result.map(|()| true)
}

/// Fetches the first URL that answers into `target`, retrying transient failures, and checks the
/// sha256 when one is pinned.
fn download(urls: &[&str], target: &Path, sha256: Option<&str>) -> Result<(), String> {
    println!("cargo:rerun-if-env-changed=REDJS_PROXY");
    let mut config = ureq::Agent::config_builder()
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::NativeTls)
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .build(),
        )
        .timeout_connect(Some(Duration::from_secs(15)))
        .user_agent("redjs-build");
    if let Some(proxy) = env::var("REDJS_PROXY").ok().filter(|v| !v.is_empty()) {
        config = config.proxy(Some(
            ureq::Proxy::new(&proxy).map_err(|e| format!("REDJS_PROXY: {e}"))?,
        ));
    }
    let agent: ureq::Agent = config.build().into();
    let mut last = String::new();
    let mut source = 0;
    for attempt in 0..4 {
        let url = urls[source];
        if attempt > 0 {
            std::thread::sleep(Duration::from_secs(1 << attempt));
        }
        match fetch(&agent, url, target) {
            Ok(digest) => {
                return match sha256 {
                    Some(want) if !digest.eq_ignore_ascii_case(want) => Err(format!(
                        "sha256 mismatch for {url}: pinned {want}, got {digest}"
                    )),
                    _ => Ok(()),
                };
            }
            Err((why, retry)) => {
                last = format!("{url}: {why}");
                if source + 1 < urls.len() {
                    source += 1;
                } else if !retry {
                    break;
                }
            }
        }
    }
    Err(last)
}

fn fetch(agent: &ureq::Agent, url: &str, target: &Path) -> Result<String, (String, bool)> {
    let response = agent.get(url).call().map_err(|e| {
        let retry = !matches!(&e, ureq::Error::StatusCode(code) if *code < 500 && *code != 408 && *code != 429);
        (e.to_string(), retry)
    })?;
    let mut body = response.into_body().into_reader();
    let mut file = File::create(target).map_err(|e| (e.to_string(), false))?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0; 1 << 20];
    loop {
        let n = match body.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err((e.to_string(), true)),
        };
        hash.update(&buffer[..n]);
        file.write_all(&buffer[..n])
            .map_err(|e| (e.to_string(), false))?;
    }
    Ok(hex(&hash.finalize()))
}

/// Unpacks the regular files of a tar.gz whose entries share one top directory (`top`, or any
/// when empty) into `dest`, keeping those whose path below it `keep` accepts. Links and special
/// files are refused: the pinned archives contain none.
fn extract_tar(
    archive: &Path,
    dest: &Path,
    top: &str,
    keep: impl Fn(&[String]) -> bool,
) -> Result<(), String> {
    let file = File::open(archive).map_err(|e| e.to_string())?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(BufReader::with_capacity(
        1 << 20,
        file,
    )));
    let mut prefix: Option<String> = (!top.is_empty()).then(|| top.to_owned());
    for entry in tar.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        let kind = entry.header().entry_type();
        if matches!(kind, tar::EntryType::XGlobalHeader) {
            continue;
        }
        let path = entry.path().map_err(|e| e.to_string())?.into_owned();
        let parts = safe_parts(&path)?;
        let first = prefix.get_or_insert_with(|| parts[0].clone());
        if parts[0] != *first {
            return Err(format!("unexpected archive entry {}", path.display()));
        }
        if !matches!(kind, tar::EntryType::Regular | tar::EntryType::Directory) {
            return Err(format!(
                "unexpected link or special file {}",
                path.display()
            ));
        }
        if parts.len() < 2 || !keep(&parts[1..]) || kind == tar::EntryType::Directory {
            continue;
        }
        let target = parts[1..]
            .iter()
            .fold(dest.to_path_buf(), |p, part| p.join(part));
        fs::create_dir_all(target.parent().unwrap()).map_err(|e| e.to_string())?;
        let mut out = File::create(&target).map_err(|e| format!("{}: {e}", target.display()))?;
        io::copy(&mut entry, &mut out).map_err(|e| format!("{}: {e}", target.display()))?;
    }
    Ok(())
}

/// Unpacks a zip, minus the SQLite shell (a program, never compiled).
fn extract_zip(archive: &Path, dest: &Path) -> Result<(), String> {
    let file = File::open(archive).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(BufReader::new(file)).map_err(|e| e.to_string())?;
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index).map_err(|e| e.to_string())?;
        let name = entry.name().map_err(|e| e.to_string())?.into_owned();
        let parts = safe_parts(Path::new(&name))?;
        if entry.is_dir() {
            continue;
        }
        if entry.is_symlink() {
            return Err(format!("unexpected link {name}"));
        }
        let leaf = parts.last().unwrap();
        if leaf.starts_with("shell") && leaf.ends_with(".c") {
            continue;
        }
        let target = parts
            .iter()
            .fold(dest.to_path_buf(), |p, part| p.join(part));
        fs::create_dir_all(target.parent().unwrap()).map_err(|e| e.to_string())?;
        let mut out = File::create(&target).map_err(|e| e.to_string())?;
        io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn safe_parts(path: &Path) -> Result<Vec<String>, String> {
    let raw = path.to_string_lossy();
    let parts: Vec<String> = path
        .components()
        .map(|c| match c {
            Component::Normal(part) => part.to_str().map(str::to_owned),
            _ => None,
        })
        .collect::<Option<_>>()
        .unwrap_or_default();
    if parts.is_empty() || raw.contains(['\\', ':', '\0', '\r', '\n']) {
        return Err(format!("unsafe archive path {raw}"));
    }
    Ok(parts)
}

/// sha256 over `<sha256 of file>  <relative/path>\n` per file, paths in byte order: the
/// `sha256sum` manifest form `*.tree-sha256` pins use.
fn verify_tree(root: &Path, want: &str, what: &str) -> Result<(), String> {
    fn walk(dir: &Path, prefix: &str, files: &mut Vec<(String, PathBuf)>) -> io::Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let name = format!("{prefix}{}", entry.file_name().to_string_lossy());
            if entry.file_type()?.is_dir() {
                walk(&entry.path(), &format!("{name}/"), files)?;
            } else {
                files.push((name, entry.path()));
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    walk(root, "", &mut files).map_err(|e| e.to_string())?;
    files.sort();
    let mut manifest = Sha256::new();
    for (name, path) in files {
        let mut hash = Sha256::new();
        io::copy(
            &mut File::open(&path).map_err(|e| e.to_string())?,
            &mut hash,
        )
        .map_err(|e| e.to_string())?;
        manifest.update(format!("{}  {name}\n", hex(&hash.finalize())));
    }
    let have = hex(&manifest.finalize());
    if have != want {
        return Err(format!(
            "{what} tree sha256 mismatch: pinned {want}, got {have}"
        ));
    }
    Ok(())
}

fn configuration(webkit: &Path) -> String {
    text(&webkit.join("include").join("cmakeconfig.h"))
}

fn text(path: &Path) -> String {
    fs::read_to_string(path)
        .unwrap_or_default()
        .replace("\r\n", "\n")
}

fn stamp(dir: &Path) -> String {
    fs::read_to_string(dir.join(".redjs-pin"))
        .map(|s| s.trim().to_owned())
        .unwrap_or_default()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
