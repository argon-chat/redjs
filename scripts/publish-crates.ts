// Publishes the crates of the redjs workspace (redjs, redjs-macros, redjs-webgpu) to crates.io, in
// dependency order.
//
//   bun scripts/publish-crates.ts --tag v0.3.1 [--dry-run] [--allow-dirty]
//
// Every published crate must be at the tag's version. The ones crates.io does not have yet are
// packaged and verified first (`cargo package`), so a broken package stops the release before
// anything is uploaded; --dry-run stops there. A version crates.io already has is skipped, so a
// re-run carries on where the last one stopped. crates.io answers a burst with 429 and the time to
// retry after (updates: 30, then one a minute; new crates: 5, then one per 10 minutes); the script
// waits that long.

import { spawnSync } from "node:child_process";
import { resolve } from "node:path";
import { parseArgs } from "node:util";

const WORKSPACE = resolve(import.meta.dir, "../redjs");

const { values } = parseArgs({
  options: { tag: { type: "string" }, "dry-run": { type: "boolean" }, "allow-dirty": { type: "boolean" } },
});
if (!values.tag) throw new Error("--tag v<version>");
const version = values.tag.replace(/^v/, "");
const dirty = values["allow-dirty"] ? ["--allow-dirty"] : [];

interface Package {
  name: string;
  version: string;
  publish: string[] | null;
  dependencies: { name: string; req: string }[];
}

const meta = spawnSync("cargo", ["metadata", "--no-deps", "--format-version", "1"], { cwd: WORKSPACE, encoding: "utf8" });
if (meta.status !== 0) throw new Error(`cargo metadata failed:\n${meta.stderr}`);
const crates = (JSON.parse(meta.stdout) as { packages: Package[] }).packages.filter(
  (p) => p.publish === null || p.publish.length > 0,
);

// A dependency without a version requirement (path-only, as the dev-dependency cycles are) is
// dropped from the published manifest, so it does not order.
function ordered(crates: Package[]): Package[] {
  const byName = new Map(crates.map((c) => [c.name, c]));
  const done = new Set<string>();
  const out: Package[] = [];
  const visit = (c: Package, stack: string[]) => {
    if (done.has(c.name)) return;
    if (stack.includes(c.name)) throw new Error(`dependency cycle: ${[...stack, c.name].join(" -> ")}`);
    for (const d of c.dependencies) {
      const dep = byName.get(d.name);
      if (dep && d.req !== "*") visit(dep, [...stack, c.name]);
    }
    done.add(c.name);
    out.push(c);
  };
  for (const c of crates) visit(c, []);
  return out;
}

async function published(name: string, version: string): Promise<boolean> {
  const res = await fetch(`https://crates.io/api/v1/crates/${name}/${version}`, {
    headers: { "User-Agent": "skathi-release (https://github.com/argon-chat/skathi)" },
  });
  if (res.status === 404) return false;
  if (res.ok) return true;
  throw new Error(`crates.io answered ${res.status} for ${name}@${version}`);
}

const off = crates.filter((c) => c.version !== version);
if (off.length) {
  throw new Error(
    `not at ${version}: ${off.map((c) => `${c.name}@${c.version}`).join(", ")}; bump the workspace version first`,
  );
}

const pending: Package[] = [];
for (const c of ordered(crates)) {
  if (await published(c.name, c.version)) console.log(`${c.name}@${c.version} is already on crates.io`);
  else pending.push(c);
}
if (!pending.length) {
  console.log("every crate is on crates.io");
  process.exit(0);
}

// The crates already published are taken from crates.io, as a user of these would get them.
const pack = spawnSync("cargo", ["package", ...pending.flatMap((c) => ["-p", c.name]), ...dirty], {
  cwd: WORKSPACE,
  stdio: "inherit",
});
if (pack.status !== 0) process.exit(pack.status ?? 1);
if (values["dry-run"]) {
  console.log(`\nwould publish: ${pending.map((c) => `${c.name}@${c.version}`).join(", ")}`);
  process.exit(0);
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
for (const c of pending) {
  for (;;) {
    console.log(`\n== publishing ${c.name}@${c.version}`);
    const r = spawnSync("cargo", ["publish", "--no-verify", "-p", c.name, ...dirty], {
      cwd: WORKSPACE,
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
    });
    const out = `${r.stdout ?? ""}${r.stderr ?? ""}`;
    process.stdout.write(out);
    if (r.status === 0) break;
    const retry = out.match(/429[\s\S]*?try again after ([^.\n]+(?:GMT|UTC|Z))/i);
    if (!retry) process.exit(r.status ?? 1);
    const until = Date.parse(retry[1]!);
    const wait = Math.max(30_000, (Number.isNaN(until) ? Date.now() + 600_000 : until) - Date.now() + 15_000);
    console.log(`crates.io rate limit: waiting ${Math.round(wait / 60_000)} min`);
    await sleep(wait);
  }
}
console.log("\nevery crate is on crates.io");
