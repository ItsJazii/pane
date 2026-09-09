import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { test } from "node:test";
import ts from "typescript";

const source = await readFile(new URL("../src/sub2api-display.ts", import.meta.url), "utf8");
const compiled = ts.transpileModule(source, { compilerOptions: { module: ts.ModuleKind.ESNext } }).outputText;
const { sub2ApiOnDemand, sub2ApiPrimaryMetric, reconcileSub2ApiLayout, sub2ApiLiveLayout, Sub2ApiSnapshotContexts, sub2ApiStatusDetails } = await import(`data:text/javascript;base64,${Buffer.from(compiled).toString("base64")}`);
const metric = (label, percent = null) => ({ label, kind: percent === null ? "text" : "progress", used_percent: percent });

test("primary information stays visible while all six Key summaries fold", () => {
  const primary = ["Total quota", "5h", "1d", "7d", "Daily", "Weekly", "Monthly", "Balance", "Expiry", "Status", "Subscription", "Type", "Remaining amount"];
  assert.ok(primary.every((label) => !sub2ApiOnDemand(label)));
  const summary = ["Today requests", "Today tokens", "Today actual cost", "Total requests", "Total tokens", "Total actual cost"];
  assert.ok(summary.every(sub2ApiOnDemand));
});

test("partial refresh preserves dormant row preferences until the allowance returns", () => {
  const layout = { metricOrder: ["5h", "Total quota", "Today requests"], onDemand: ["5h", "Today requests"], hidden: ["5h"], starred: ["5h"] };
  const saved = structuredClone(layout);
  reconcileSub2ApiLayout([metric("Total quota", 10), metric("Today requests")], layout);
  assert.deepEqual(layout, saved);
  reconcileSub2ApiLayout([metric("Total quota", 20), metric("5h", 60), metric("Today requests")], layout);
  assert.deepEqual(layout, saved);
});

test("every mode switch retains preferences for returning rows", () => {
  const modes = [
    [metric("Balance"), metric("Status")],
    [metric("Total quota", 10), metric("5h", 90), metric("Expiry")],
    [metric("Daily", 30), metric("Weekly", 40), metric("Subscription")],
  ];
  for (const before of modes) for (const after of modes) {
    const layout = { metricOrder: [...before.map((m) => m.label), "Today requests"], onDemand: ["Today requests"], hidden: ["Today requests"], starred: [] };
    const current = [...after, metric("Today requests")];
    reconcileSub2ApiLayout(current, layout);
    assert.deepEqual(new Set(layout.metricOrder), new Set([...before, ...current].map((m) => m.label)));
    assert.deepEqual(layout.onDemand, ["Today requests"]);
    assert.deepEqual(layout.hidden, ["Today requests"]);
    assert.equal(reconcileSub2ApiLayout(current, layout), false);
  }
});

test("highest valid quota wins with stable standard-order ties, including overage", () => {
  const rows = [metric("7d", 80), metric("Total quota", 80), metric("5h", 70)];
  assert.equal(sub2ApiPrimaryMetric(rows)?.label, "Total quota");
  assert.equal(sub2ApiPrimaryMetric([...rows, metric("1d", 150)])?.label, "1d");
  assert.equal(sub2ApiPrimaryMetric([metric("Daily", NaN), metric("Weekly", 0)])?.label, "Weekly");
  assert.equal(sub2ApiPrimaryMetric([metric("Balance")])?.used_percent, null);
  assert.equal(sub2ApiPrimaryMetric([metric("Remaining amount")])?.label, "Remaining amount");
  assert.equal(sub2ApiPrimaryMetric([metric("Total quota", Infinity)]), undefined);
});

test("display projection hides dormant rows and stars without changing saved choices", () => {
  const layout = { metricOrder: ["5h", "Balance", "Daily", "Today requests"], hidden: ["5h"], onDemand: ["Today requests"], starred: ["5h"] };
  const saved = structuredClone(layout);
  const current = [metric("Daily", 40), metric("Today requests")];
  assert.deepEqual(sub2ApiLiveLayout(current, layout), {
    metricOrder: ["Daily", "Today requests"], hidden: [], onDemand: ["Today requests"], starred: [],
  });
  assert.deepEqual(sub2ApiLiveLayout([metric("5h", 60)], layout), {
    metricOrder: ["5h"], hidden: ["5h"], onDemand: [], starred: ["5h"],
  });
  assert.deepEqual(layout, saved);
});

test("partial card keeps available content visible without rewriting on-demand choices", () => {
  const layout = { metricOrder: ["5h", "Today requests"], hidden: [], onDemand: ["Today requests"], starred: [] };
  const saved = structuredClone(layout);
  assert.deepEqual(sub2ApiLiveLayout([metric("Today requests")], layout).onDemand, []);
  assert.deepEqual(sub2ApiLiveLayout([metric("5h", 40), metric("Today requests")], layout).onDemand, ["Today requests"]);
  assert.deepEqual(layout, saved);
});

test("restoring allowances keeps two active stars while retaining every saved preference", () => {
  const layout = { metricOrder: ["5h", "1d", "Daily", "Weekly"], hidden: [], onDemand: [], starred: ["5h", "1d"] };
  const partial = [metric("Daily", 20), metric("Weekly", 30)];
  assert.deepEqual(sub2ApiLiveLayout(partial, layout).starred, []);
  layout.starred.push("Daily", "Weekly");
  assert.deepEqual(sub2ApiLiveLayout(partial, layout).starred, ["Daily", "Weekly"]);
  const restored = [metric("5h", 40), metric("1d", 50), ...partial];
  assert.deepEqual(sub2ApiLiveLayout(restored, layout).starred, ["5h", "1d"]);
  assert.deepEqual(layout.starred, ["5h", "1d", "Daily", "Weekly"]);
});

test("late refresh cannot restore rotated, migrated, or deleted Key data", async () => {
  for (const affected of [["sub2api@a"], ["sub2api@a", "sub2api@b"]]) {
    const contexts = new Sub2ApiSnapshotContexts();
    const started = contexts.capture();
    const rows = ["sub2api@a", "sub2api@b", "claude"].map((id) => ({ id, name: id, metrics: [metric("Balance")] }));
    // The response arrived, then a config await allowed a mutation to finish.
    const response = await Promise.resolve(rows);
    contexts.invalidate(affected);
    const current = rows.filter((row) => !affected.includes(row.id));
    assert.deepEqual(contexts.publish(response, started, current).map((row) => row.id), current.map((row) => row.id));
    // A subsequent refresh under the new context may publish data again.
    assert.deepEqual(contexts.publish(rows, contexts.capture(), current), rows);
  }
});

test("late refresh keeps renamed data and cannot overwrite its new name", () => {
  const contexts = new Sub2ApiSnapshotContexts();
  const started = contexts.capture();
  const old = { id: "sub2api@a", name: "Old site · Old key", metrics: [metric("Daily", 50)] };
  contexts.invalidate([old.id]);
  const renamed = { ...old, name: "New site · New key" };
  assert.deepEqual(contexts.publish([old], started, [renamed]), [renamed]);
  // Backend may discard this result itself: preserve the current renamed card.
  assert.deepEqual(contexts.publish([], started, [renamed]), [renamed]);
});

test("strip tooltip includes valid restriction status separately from percentages", () => {
  assert.deepEqual(sub2ApiStatusDetails([{ ...metric("Status"), value: "Expired" }]), ["Expired"]);
  assert.deepEqual(sub2ApiStatusDetails([metric("Daily", 100), { ...metric("Status"), value: "Quota exhausted" }]), ["Quota exhausted"]);
  assert.deepEqual(sub2ApiStatusDetails([metric("Balance")]), []);
});

test("combined wallet status translates every known restriction", async () => {
  const source = await readFile(new URL("../src/i18n.ts", import.meta.url), "utf8");
  const code = ts.transpileModule(source, { compilerOptions: { module: ts.ModuleKind.ESNext } }).outputText;
  const { setActiveLocale, displayMetricDetail } = await import(`data:text/javascript;base64,${Buffer.from(code).toString("base64")}`);
  setActiveLocale("zh");
  assert.equal(displayMetricDetail("Expired · Overdue"), "已过期 · 欠费");
  assert.equal(displayMetricDetail("Disabled · Overdue"), "已禁用 · 欠费");
  assert.equal(displayMetricDetail("Private plan · Unrecognized status"), "Private plan · Unrecognized status");
});

test("partial quota card text retains and localizes its explicit reset time", async () => {
  const source = await readFile(new URL("../src/i18n.ts", import.meta.url), "utf8");
  const code = ts.transpileModule(source, { compilerOptions: { module: ts.ModuleKind.ESNext } }).outputText;
  const { setActiveLocale, displayMetricDetail } = await import(`data:text/javascript;base64,${Buffer.from(code).toString("base64")}`);
  const text = "Unknown of $10.00 · Resets 2030-01-01 00:00 UTC";
  setActiveLocale("zh");
  assert.equal(displayMetricDetail(text), "Unknown of $10.00 · 2030-01-01 00:00 UTC 重置");
  setActiveLocale("en");
  assert.equal(displayMetricDetail(text), text);
});
