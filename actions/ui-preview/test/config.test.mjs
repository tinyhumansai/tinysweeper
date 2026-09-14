import { test } from "node:test";
import assert from "node:assert/strict";

import { checkConfig, DEFAULTS } from "../src/config.mjs";

test("an empty config is the defaults", () => {
  const c = checkConfig({});
  assert.equal(c.serve, DEFAULTS.serve);
  assert.deepEqual(c.viewport, [1440, 900]);
  assert.deepEqual(c.auth, { cookies: [], localStorage: {} });
});

test("auth is merged rather than replaced", () => {
  const c = checkConfig({ auth: { localStorage: { token: "t" } } });
  assert.deepEqual(c.auth.cookies, []);
  assert.equal(c.auth.localStorage.token, "t");
});

test("the mistakes that would surface as playwright errors are refused up front", () => {
  assert.throws(() => checkConfig({ ready: "index.html" }), /"ready"/);
  assert.throws(() => checkConfig({ viewport: [100, 100] }), /"viewport"/);
  assert.throws(() => checkConfig({ entry_points: [{ name: "x", path: "settings" }] }), /entry point/);
  assert.throws(() => checkConfig({ mocks: [{ url: "**/api/**" }] }), /needs a "dir"/);
  assert.throws(() => checkConfig({ auth: { cookies: [{ name: "sid" }] } }), /auth cookie/);
  assert.throws(() => checkConfig({ max_flows: 0 }), /"max_flows"/);
});
