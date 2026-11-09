import { existsSync, lstatSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, realpathSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { dirname, isAbsolute, join, relative, resolve, sep } from "node:path";
import { parseArgs } from "node:util";

const ROOT = realpathSync(resolve(import.meta.dir, ".."));
const decoder = new TextDecoder();
const TABLE_FILES = ["AOTBuiltinTableAttachments.cpp", "AOTBuiltinTableBuiltins.cpp", "AOTBuiltinTableNames.cpp", "AOTBuiltinTableOverloads.cpp", "AOTBuiltinTableTypes.cpp", "AOTWire.cpp"];
const TABLE_HEADERS = ["AOTBuiltinTableArrays.h", "AOTBuiltinNameSlots.h", "AOTBuiltinTableEnums.h", "AOTSchemaInlineBuiltins.h"];

export function readPins(): Record<string, string> {
  const pins = Object.fromEntries(readFileSync(join(ROOT, "pins.txt"), "utf8").split(/\r?\n/).flatMap(raw => {
    const line = raw.split("#", 1)[0]!.trim();
    if (!line) return [];
    const equals = line.indexOf("=");
    if (equals < 1) throw new Error(`Invalid pin: ${line}`);
    return [[line.slice(0, equals).trim(), line.slice(equals + 1).trim()]];
  }));
  for (const key of ["webkit.tag", "webkit.version", "webkit.aot", "webkit.asset", "webkit.sha256",
    "mimalloc.commit", "mimalloc.tree-sha256", "sqlite3mc.version", "sqlite3mc.asset", "sqlite3mc.sha256", "sqlite3mc.tree-sha256"]) {
    if (!pins[key]) throw new Error(`Missing pin: ${key}`);
  }
  for (const [key, value] of Object.entries(pins)) {
    if (key.endsWith("sha256") && !/^[0-9a-f]{64}$/.test(value)) throw new Error(`Invalid SHA-256 pin: ${key}`);
  }
  return pins;
}

export async function digest(path: string): Promise<string> {
  const hash = new Bun.CryptoHasher("sha256");
  for await (const chunk of Bun.file(path).stream()) hash.update(chunk);
  return hash.digest("hex");
}

function filesUnder(root: string, directory = root): string[] {
  return readdirSync(directory, { withFileTypes: true }).flatMap(entry => {
    const path = join(directory, entry.name);
    if (entry.isSymbolicLink()) throw new Error(`Unexpected source symlink: ${path}`);
    return entry.isDirectory() ? filesUnder(root, path) : entry.isFile() ? [relative(root, path).split(sep).join("/")] : [];
  });
}

export async function treeDigest(root: string): Promise<string> {
  const hash = new Bun.CryptoHasher("sha256");
  for (const name of filesUnder(root).sort()) hash.update(`${await digest(join(root, name))}  ${name}\n`);
  return hash.digest("hex");
}

function command(args: string[]): string {
  const result = Bun.spawnSync(args, { stdout: "pipe", stderr: "pipe" });
  if (result.exitCode !== 0) throw new Error(`${args[0]} failed: ${decoder.decode(result.stderr)}`);
  return decoder.decode(result.stdout);
}

class DownloadError extends Error {
  constructor(message: string, readonly retryable: boolean) { super(message); }
}

function curl(url: string, target: string, headers: string[] = []) {
  const result = Bun.spawnSync([
    "curl", "--fail", "--location", "--silent", "--show-error", "--connect-timeout", "15",
    "--speed-limit", "1024", "--speed-time", "60", "--output", target, "--write-out", "%{http_code}",
    ...headers.flatMap(header => ["--header", header]), url,
  ], { stdout: "pipe", stderr: "pipe" });
  return { code: result.exitCode, status: Number(decoder.decode(result.stdout)), error: decoder.decode(result.stderr).trim() };
}

export async function download(url: string, target: string, expected?: string, mirrors: string[] = []) {
  const sources = [url, ...mirrors];
  for (let attempt = 0; attempt < 3; attempt++) {
    const source = sources[Math.min(attempt, sources.length - 1)]!;
    console.log(`[redjs] downloading ${source}`);
    try {
      let result = curl(source, target);
      const release = /^https:\/\/github\.com\/([^/]+\/[^/]+)\/releases\/download\/([^/]+)\/([^/]+)$/.exec(source);
      if (result.code !== 0 && release && (result.status >= 500 || result.status === 0)) {
        // Public API fallback for a failed release CDN; the pinned digest is
        // mandatory regardless of which endpoint supplied the same archive.
        const metadata = JSON.parse(command(["curl", "-fsSL", "--retry", "2", "--connect-timeout", "15", "--max-time", "60",
          `https://api.github.com/repos/${release[1]}/releases/tags/${release[2]}`]));
        const asset = metadata.assets.find((asset: { name: string }) => asset.name === release[3]);
        if (!asset) throw new Error(`Pinned release asset missing: ${source}`);
        result = curl(asset.url, target, ["Accept: application/octet-stream"]);
      }
      if (result.code !== 0) throw new DownloadError(result.error, result.status === 0 || result.status >= 500 || [408, 429].includes(result.status) || result.code !== 22);
      if (expected && await digest(target) !== expected) throw new Error(`SHA-256 mismatch: ${source}`);
      return;
    } catch (error) {
      if (!(error instanceof DownloadError) || !error.retryable || attempt === 2) throw error;
      rmSync(target, { force: true });
      console.log(`[redjs] transient download failure, retrying: ${error.message}`);
      await Bun.sleep(1000 * 2 ** attempt);
    }
  }
}

function safeArchivePath(name: string): string[] {
  const parts = name.replace(/\/$/, "").split("/");
  if (!name || name.startsWith("/") || name.includes("\\") || name.includes(":") || /[\r\n\0]/.test(name)
      || parts.some(part => !part || part === "." || part === "..")) throw new Error(`Unsafe archive path: ${name}`);
  return parts;
}

function extractKnownTar(archive: string, destination: string, members: string[] = []) {
  for (const name of command(["tar", "-tzf", archive]).trimEnd().split("\n")) {
    if (safeArchivePath(name)[0] !== "bun-webkit") throw new Error(`Unexpected WebKit prefix: ${name}`);
  }
  // The pinned native archives contain only regular files and directories.
  // Reject links before extracting, including links through parent directories.
  if (command(["tar", "-tvzf", archive]).split("\n").some(line => line && !["-", "d"].includes(line[0]!))) {
    throw new Error("Unexpected link or special file in pinned WebKit archive");
  }
  mkdirSync(destination, { recursive: true });
  command(["tar", "-xzf", archive, "-C", destination, "--strip-components=1", ...members]);
}

export async function extractMimalloc(archive: string, destination: string) {
  const files = await new Bun.Archive(await Bun.file(archive).bytes()).files();
  let top: string | undefined;
  for (const [name, file] of files) {
    const [prefix, ...parts] = safeArchivePath(name);
    top ??= prefix;
    if (prefix !== top) throw new Error(`Unexpected mimalloc prefix: ${name}`);
    if (!parts.length || !["src", "include", "LICENSE"].includes(parts[0]!)) continue;
    const target = join(destination, ...parts);
    mkdirSync(dirname(target), { recursive: true });
    // Copy file bytes rather than extracting archive links. The resulting
    // regular-file tree must match the pinned manifest before publication.
    await Bun.write(target, file);
  }
}

export function extractSqlite(archive: string, destination: string) {
  for (const name of command(["unzip", "-Z1", archive]).trimEnd().split("\n")) safeArchivePath(name);
  mkdirSync(destination, { recursive: true });
  command(["unzip", "-q", archive, "-d", destination]);
  for (const name of filesUnder(destination)) if (/^shell.*\.c$/.test(name.split("/").at(-1)!)) rmSync(join(destination, name));
}

function inside(root: string, path: string): boolean {
  const child = relative(root, resolve(path));
  return child !== "" && child !== ".." && !child.startsWith(`..${sep}`) && !isAbsolute(child);
}

function assertScoped(path: string) {
  if (!inside(ROOT, path)) throw new Error(`Native input path is outside redjs: ${path}`);
  let ancestor = resolve(path);
  while (!existsSync(ancestor)) ancestor = dirname(ancestor);
  if (lstatSync(ancestor).isSymbolicLink()) throw new Error(`Refusing native input symlink: ${ancestor}`);
  const actual = realpathSync(ancestor);
  if (actual !== ROOT && !inside(ROOT, actual)) throw new Error(`Native input resolves outside redjs: ${path}`);
}

function publish(staged: string, destination: string) {
  assertScoped(staged);
  assertScoped(destination);
  mkdirSync(dirname(destination), { recursive: true });
  rmSync(destination, { recursive: true, force: true });
  renameSync(staged, destination);
}

function text(path: string) { return readFileSync(path, "utf8").replace(/\r\n?/g, "\n"); }

export async function setup(options: { platform?: string; webkitTarball?: string; tableTarball?: string } = {}) {
  const pin = readPins();
  const os = process.platform === "darwin" ? "macos" : process.platform;
  const arch = process.arch === "x64" ? "amd64" : process.arch;
  const platform = options.platform ?? `${os}-${arch}`;
  if (!pin[`webkit.${platform}.asset`]) throw new Error(`No pinned Unix prebuilt for ${platform}; use setup.ps1 on Windows`);
  if (!pin[`webkit.${platform}.sha256`]) throw new Error(`Missing WebKit SHA-256 pin for ${platform}`);
  const prebuilt = join(ROOT, "prebuilt");
  assertScoped(prebuilt);
  mkdirSync(prebuilt, { recursive: true });
  // Staging stays on the destination filesystem, so publication is a rename.
  const scratch = mkdtempSync(join(prebuilt, ".redjs-setup-"));
  try {
    const webkit = join(prebuilt, `bun-webkit-${platform}`);
    const configuration = join(webkit, "include/cmakeconfig.h");
    const version = `#define BUN_WEBKIT_VERSION "${pin["webkit.version"]}"`;
    const aot = `#define BUN_WEBKIT_ENABLE_AOT ${pin["webkit.aot"]}`;
    const ready = existsSync(configuration) && text(configuration).includes(version) && text(configuration).includes(aot);
    if (!ready || !existsSync(join(webkit, "lib/libJavaScriptCore.a"))) {
      const archive = options.webkitTarball ?? join(scratch, "webkit.tar.gz");
      const expected = pin[`webkit.${platform}.sha256`]!;
      if (options.webkitTarball) {
        if (await digest(archive) !== expected) throw new Error("WebKit tarball SHA-256 does not match pins.txt");
      } else {
        await download(`https://github.com/oven-sh/WebKit/releases/download/${pin["webkit.tag"]}/${pin[`webkit.${platform}.asset`]}`, archive, expected);
      }
      const staged = join(scratch, "webkit");
      extractKnownTar(archive, staged);
      const config = text(join(staged, "include/cmakeconfig.h"));
      if (!config.includes(version) || !config.includes(aot) || !existsSync(join(staged, "lib/libJavaScriptCore.a"))) throw new Error("Wrong WebKit engine, AOT variant, or library");
      publish(staged, webkit);
    }

    // Unix archives omit frontend table sources. Obtain portable sources from
    // the verified Windows archive of the same engine, checking all ABI headers.
    const generated = join(webkit, "include/JavaScriptCore/aot");
    if (!TABLE_FILES.every(name => existsSync(join(generated, name)))) {
      const archive = options.tableTarball ?? join(scratch, "builtin-tables.tar.gz");
      if (options.tableTarball) {
        if (await digest(archive) !== pin["webkit.sha256"]) throw new Error("Builtin table archive SHA-256 mismatch");
      } else {
        await download(`https://github.com/oven-sh/WebKit/releases/download/${pin["webkit.tag"]}/${pin["webkit.asset"]}`, archive, pin["webkit.sha256"]);
      }
      const staged = join(scratch, "builtin-tables");
      extractKnownTar(archive, staged, ["bun-webkit/include"]);
      const include = join(staged, "include");
      if (!text(join(include, "cmakeconfig.h")).includes(version)) throw new Error("Builtin table engine version mismatch");
      for (const name of TABLE_HEADERS) if (text(join(include, "JavaScriptCore", name)) !== text(join(webkit, "include/JavaScriptCore", name))) throw new Error(`Builtin table ABI mismatch: ${name}`);
      if (!TABLE_FILES.every(name => existsSync(join(include, "JavaScriptCore/aot", name)))) throw new Error("Incomplete builtin table archive");
      publish(join(include, "JavaScriptCore/aot"), generated);
    }

    const mimalloc = join(ROOT, "redjs/cpp/mimalloc");
    const miStamp = join(mimalloc, ".redjs-pin");
    const commit = pin["mimalloc.commit"]!;
    if (!existsSync(miStamp) || text(miStamp).trim() !== commit || !existsSync(join(mimalloc, "src/static.c"))) {
      const archive = join(scratch, "mimalloc.tar.gz");
      await download(`https://api.github.com/repos/oven-sh/mimalloc/tarball/${commit}`, archive, undefined,
        [`https://codeload.github.com/oven-sh/mimalloc/tar.gz/${commit}`, `https://github.com/oven-sh/mimalloc/archive/${commit}.tar.gz`]);
      const staged = join(scratch, "mimalloc");
      await extractMimalloc(archive, staged);
      if (await treeDigest(staged) !== pin["mimalloc.tree-sha256"]) throw new Error("Mimalloc tree SHA-256 mismatch");
      writeFileSync(join(staged, ".redjs-pin"), commit);
      publish(staged, mimalloc);
    }

    const sqlite = join(prebuilt, "sqlite3mc");
    const mcStamp = join(sqlite, ".redjs-pin");
    const receipt = `${pin["sqlite3mc.version"]} ${pin["sqlite3mc.tree-sha256"]}`;
    if (!existsSync(mcStamp) || text(mcStamp).trim() !== receipt || !existsSync(join(sqlite, "sqlite3mc_amalgamation.c"))) {
      const archive = join(scratch, "sqlite.zip");
      await download(`https://github.com/utelle/SQLite3MultipleCiphers/releases/download/v${pin["sqlite3mc.version"]}/${pin["sqlite3mc.asset"]}`, archive, pin["sqlite3mc.sha256"]);
      const staged = join(scratch, "sqlite");
      extractSqlite(archive, staged);
      if (await treeDigest(staged) !== pin["sqlite3mc.tree-sha256"]) throw new Error("SQLite tree SHA-256 mismatch");
      writeFileSync(join(staged, ".redjs-pin"), receipt);
      publish(staged, sqlite);
    }
    console.log(`[redjs] verified native inputs ready for ${platform}`);
  } finally {
    assertScoped(scratch);
    rmSync(scratch, { recursive: true, force: true });
  }
}

if (import.meta.main) {
  const { values } = parseArgs({ args: Bun.argv.slice(2), options: {
    "webkit-tarball": { type: "string" }, "table-tarball": { type: "string" }, platform: { type: "string" },
  }});
  await setup({ platform: values.platform, webkitTarball: values["webkit-tarball"], tableTarball: values["table-tarball"] });
}
