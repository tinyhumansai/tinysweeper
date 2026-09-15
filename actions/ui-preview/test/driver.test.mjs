import { test } from "node:test";
import assert from "node:assert/strict";

import { resolveLocator, execute } from "../src/driver.mjs";

/** A page that records which locator method was asked for. */
function fakePage() {
  const asked = [];
  const locator = {
    first: () => locator,
    click: async () => asked.push("click"),
    fill: async (v) => asked.push(`fill:${v}`),
    boundingBox: async () => ({ x: 10, y: 20, width: 30, height: 40 }),
  };
  return {
    asked,
    getByRole: (role, o) => (asked.push(`role:${role}:${o.name ?? ""}:${o.exact ?? ""}`), locator),
    getByLabel: (t) => (asked.push(`label:${t}`), locator),
    getByText: (t, o) => (asked.push(`text:${t}:${o.exact}`), locator),
    getByPlaceholder: (t) => (asked.push(`placeholder:${t}`), locator),
    getByTestId: (t) => (asked.push(`testid:${t}`), locator),
    goto: async (url) => asked.push(`goto:${url}`),
    waitForLoadState: async () => {},
    evaluate: async () => 100,
    screenshot: async () => Buffer.from("png"),
    url: () => "http://127.0.0.1:3001/settings",
    keyboard: { press: async (k) => asked.push(`press:${k}`) },
  };
}

test("every locator kind maps to its playwright method", () => {
  const page = fakePage();
  resolveLocator(page, { by: "role", role: "button", name: "Save", exact: true });
  resolveLocator(page, { by: "label", text: "Name" });
  resolveLocator(page, { by: "text", text: "Hi" });
  resolveLocator(page, { by: "placeholder", text: "Search" });
  resolveLocator(page, { by: "test_id", id: "t" });
  assert.deepEqual(page.asked, ["role:button:Save:true", "label:Name", "text:Hi:false", "placeholder:Search", "testid:t"]);
  assert.throws(() => resolveLocator(page, { by: "css", text: "div" }), /unknown locator/);
});

test("a batch runs in order, counts steps, and stops at the first failure", async () => {
  const page = fakePage();
  const ctx = { origin: "http://127.0.0.1:3001", shots: new Map(), recorder: { start() {}, stop() {} }, masks: [], scale: 2, step: 0 };
  const results = await execute(
    page,
    [
      { op: "record", start: true },
      { op: "goto", path: "/settings" },
      { op: "goto", path: "https://evil.example/" },
      { op: "click", locator: { by: "text", text: "never" } },
    ],
    ctx,
  );
  assert.deepEqual(
    results.map((r) => [r.index, r.ok]),
    [[0, true], [1, true], [2, false]],
  );
  assert.match(results[2].error, /same-origin/);
  assert.equal(ctx.step, 2, "record is not a step; the failed goto is");
  assert.ok(page.asked.includes("goto:http://127.0.0.1:3001/settings"));
  assert.ok(!page.asked.includes("click"));
});

test("annotate measures boxes in image pixels with the scroll offset added", async () => {
  const page = fakePage();
  const ctx = { origin: "http://127.0.0.1:3001", shots: new Map(), recorder: { start() {}, stop() {} }, masks: [], scale: 2, step: 3 };
  const results = await execute(
    page,
    [
      { op: "screenshot", id: "s1" },
      { op: "annotate", shot: "s1", callouts: [{ locator: { by: "test_id", id: "t" }, label: "Here" }] },
      { op: "annotate", shot: "nope", callouts: [] },
    ],
    ctx,
  );
  assert.deepEqual(results.map((r) => r.ok), [true, true, false]);
  const shot = ctx.shots.get("s1");
  assert.equal(shot.step, 3);
  assert.equal(shot.url, "http://127.0.0.1:3001/settings");
  // evaluate() answers 100 for both scroll offsets; the box is (10,20,30,40) at 2x.
  assert.deepEqual(shot.callouts, [{ n: 1, label: "Here", box: { x: 220, y: 240, w: 60, h: 80 } }]);
});

test("a multi-callout annotate that misses one target leaves the shot untouched", async () => {
  const page = fakePage();
  const missing = { first: () => missing, boundingBox: async () => null };
  page.getByText = () => missing;
  const ctx = { origin: "http://127.0.0.1:3001", shots: new Map(), recorder: { start() {}, stop() {} }, masks: [], scale: 1, step: 0 };
  const results = await execute(
    page,
    [
      { op: "screenshot", id: "s1" },
      {
        op: "annotate",
        shot: "s1",
        callouts: [
          { locator: { by: "test_id", id: "ok" }, label: "Found" },
          { locator: { by: "text", text: "gone" }, label: "Missing" },
        ],
      },
    ],
    ctx,
  );
  assert.equal(results[1].ok, false);
  assert.deepEqual(ctx.shots.get("s1").callouts, []);
});
