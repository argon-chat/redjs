// A deliberately bounded native AOT structure/GC stress fixture. This is not
// an Argon workload or a memory-limit benchmark: 100k distinct dynamic shapes
// in four rounds, at most 25k active rows plus 128 retained samples.
declare var __VN_AOT_HOST__: any;
const g: any = __VN_AOT_HOST__;
const ROUNDS: number = 4;
const PER_ROUND: number = 25000;
const RETAIN_PER_ROUND: number = 32;
let live: any[] = [];
const retained: any[] = [];
const weak: any[] = [];
let mutatedThrough: number = -1;
let created: number = 0;
let verified: number = 0;
let checksum: number = 0;
let gcCalls: number = 0;
let reentries: number = 0;
let round: number = 0;
const indexed: any = new g.Map();
function check(ok: boolean, message: string): void {
  if (!ok) throw new g.Error(message);
}
function key(id: number): string { return "shape:" + id; }
function detail(id: number): string { return "detail:" + id; }
function replacement(id: number): string { return "replacement:" + id; }
function verifyRow(row: any, changed: boolean): void {
  const id: number = row.id;
  check(row.round === ((id / PER_ROUND) | 0), "round " + id);
  check(row[key(id)] === ((id ^ 0xA5A5) + (changed ? 11 : 0)), "dynamic value " + id);
  check(row.checksum === id * 3 + (changed ? 12 : 7), "scalar " + id);
  check(g.Array.isArray(row.array) && row.array[0] === id, "array " + id);
  if (changed) {
    check(!g.Object.hasOwn(row, detail(id)) && !g.Object.hasOwn(row, "temporary"), "deleted keys " + id);
    check(row[replacement(id)] === id * 2, "replacement " + id);
    check(row.array.length === 3 && row.array[1] === id + 4 && row.array[2] === id + 2, "array churn " + id);
  } else {
    check(row[detail(id)] === "v" + id && row.temporary === id + 3, "initial keys " + id);
    check(row.array.length === 2 && row.array[1] === id + 1, "initial array " + id);
  }
}
function verifyRetained(): number {
  check(retained.length <= ROUNDS * RETAIN_PER_ROUND, "unbounded retained samples");
  check(weak.length === retained.length, "weak sample count");
  for (let i: number = 0; i < retained.length; i++) {
    verifyRow(retained[i], true);
    check(weak[i].deref() === retained[i], "strong sample lost through WeakRef");
  }
  return retained.length;
}
// Rust forces GC and calls this compiled function while its own callback and
// the enclosing compiled loop are on the stack. It never calls the host again.
g.__vn_aot_stress_reenter = (token: number): number => {
  reentries++;
  verifyRetained();
  if (live.length) {
    const positions: number[] = [0, live.length >> 1, live.length - 1];
    for (const index of positions) {
      verifyRow(live[index], index <= mutatedThrough);
      check(indexed.get(live[index].id) === live[index], "Map identity during reentry");
    }
  }
  return token ^ 0x5A5A;
};
function collect(): void {
  const token: number = created + gcCalls;
  gcCalls++;
  check(g.__vn_aot_stress_gc(token) === token + 1, "native GC callback result");
}
for (round = 0; round < ROUNDS; round++) {
  mutatedThrough = -1;
  for (let i: number = 0; i < PER_ROUND; i++) {
    const id: number = round * PER_ROUND + i;
    const row: any = { id, round, checksum: id * 3 + 7, array: [id, id + 1] };
    row[key(id)] = id ^ 0xA5A5;
    row[detail(id)] = "v" + id;
    row.temporary = id + 3;
    live.push(row);
    indexed.set(id, row);
    created++;
    if ((i + 1) % 4096 === 0) collect();
  }
  collect();
  for (let i: number = 0; i < live.length; i++) verifyRow(live[i], false);
  for (let i: number = 0; i < live.length; i++) {
    const row: any = live[i];
    const id: number = row.id;
    row[key(id)] += 11;
    delete row[detail(id)];
    row[replacement(id)] = id * 2;
    delete row.temporary;
    row.checksum += 5;
    row.array.push(id + 2);
    row.array[1] += 3;
    mutatedThrough = i;
    if ((i + 1) % 4096 === 0) collect();
  }
  collect();
  let kept: number = 0;
  for (let i: number = 0; i < live.length; i++) {
    const row: any = live[i];
    verifyRow(row, true);
    check(indexed.get(row.id) === row, "Map identity after GC");
    checksum += row.checksum;
    verified++;
    if (kept < RETAIN_PER_ROUND && i % 781 === 0) {
      retained.push(row);
      weak.push(new g.WeakRef(row));
      kept++;
    }
  }
  check(kept === RETAIN_PER_ROUND, "retained round sample count");
  live = [];
  indexed.clear();
  collect();
}
collect();
check(created === 100000 && verified === created, "object counts");
check(checksum === 15001050000, "final checksum");
check(verifyRetained() === 128 && live.length === 0 && indexed.size === 0, "bounded final retention");
g.__vn_aot_stress_verify = (): number => verifyRetained();
g.__vn_aot_stress_stats = g.JSON.stringify({
  rounds: ROUNDS, perRound: PER_ROUND, distinctObjects: created,
  verified, checksum, gcCalls, reentries, peakLiveRows: PER_ROUND,
  retainedRows: retained.length, activeRows: live.length, mapEntries: indexed.size,
});
g.__vn_aot_stress_result = "ok";
