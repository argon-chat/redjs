import { readFileSync, writeFileSync } from "node:fs";

const ENGINE = new Set(["JavaScriptCore", "WTF", "bmalloc", "icui18n", "icuuc", "icudata"]);

function words(input: string): string[] {
  const result: string[] = [];
  let word = "", quote = "", escaped = false, started = false;
  for (const char of input) {
    if (escaped) { word += char; escaped = false; continue; }
    if (char === "\\" && quote !== "'") { escaped = true; started = true; continue; }
    if (quote) { if (char === quote) quote = ""; else word += char; continue; }
    if (char === "'" || char === '"') { quote = char; started = true; continue; }
    if (/\s/.test(char)) { if (started) result.push(word); word = ""; started = false; }
    else { word += char; started = true; }
  }
  if (quote || escaped) throw new Error("Malformed rustc native-static-libs report");
  if (started) result.push(word);
  return result;
}

export function nativeLinkArgs(report: string): string[] {
  const lines = report.replace(/\x1b\[[0-?]*[ -/]*[@-~]/g, "").split(/\r?\n/).filter(line => line.includes("native-static-libs:"));
  if (!lines.length) throw new Error("rustc did not report native-static-libs; cannot stage NativeAOT link metadata");
  const args = words(lines.at(-1)!.split("native-static-libs:", 2)[1]!.trim());
  const result: string[] = [];
  for (let index = 0; index < args.length; index++) {
    let argument = args[index]!;
    if (argument.startsWith("-l") && ENGINE.has(argument.slice(2))) continue;
    if (argument === "-framework") {
      const framework = args[++index];
      if (!framework) throw new Error("Missing framework name in rustc report");
      argument = `-Wl,-framework,${framework}`;
    }
    result.push(argument);
  }
  return result;
}

if (import.meta.main) {
  if (Bun.argv[2] === "--target-directory") {
    const metadata = await Bun.stdin.json();
    if (typeof metadata.target_directory !== "string") throw new Error("Missing Cargo target_directory");
    console.log(metadata.target_directory);
  } else {
    if (Bun.argv.length !== 4) throw new Error("Usage: native-link-args.ts <rustc log> <output>");
    writeFileSync(Bun.argv[3]!, nativeLinkArgs(readFileSync(Bun.argv[2]!, "utf8")).join("\n") + "\n");
  }
}
