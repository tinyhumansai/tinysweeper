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
  assert.throws(() => checkConfig({ auth: { visit: "connect.html" } }), /"auth.visit"/);
  assert.equal(checkConfig({ auth: { visit: "/__preview-connect.html" } }).auth.visit, "/__preview-connect.html");
  assert.throws(() => checkConfig({ mocks: "not-a-list" }), /"mocks" must be a list/);
  assert.throws(() => checkConfig({ auth: { cookies: "not-a-list" } }), /"auth.cookies" must be a list/);
  assert.throws(() => checkConfig({ timeout_s: 0 }), /"timeout_s"/);
  assert.throws(() => checkConfig({ timeout_s: Infinity }), /"timeout_s"/);
  assert.throws(() => checkConfig({ mask: "not-a-list" }), /"mask" must be a list/);
});

test("a non-object top-level config is refused rather than silently defaulted", () => {
  for (const bad of [null, [], 42, "not an object"]) {
    assert.throws(() => checkConfig(bad), /top-level config must be an object/);
  }
});
