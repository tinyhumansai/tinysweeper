// The repository's ui-preview config: read, defaulted, checked.
//
// Everything a repository has to tell the runner about itself lives in one
// JSON file, so the workflow that calls the action is the same for every
// repository and the differences are data. The checks here are the ones
// whose failure would otherwise show up as a confusing Playwright error ten
// minutes into a job.

import { readFile } from "node:fs/promises";
import path from "node:path";

/** The defaults a config is laid over. */
export const DEFAULTS = Object.freeze({
  serve: "bash scripts/ui-preview/serve.sh",
  ready: "/",
  timeout_s: 420,
  viewport: [1440, 900],
  auth: { cookies: [], localStorage: {} },
  mocks: [],
  mask: [],
  entry_points: [],
  max_flows: 4,
});

/** Read and validate the config at `file`, resolved against `root`. */
export async function loadConfig(root, file) {
  const full = path.resolve(root, file);
  let raw;
  try {
    raw = await readFile(full, "utf8");
  } catch (err) {
    throw new Error(`no ui-preview config at ${full}: ${err.message}`);
  }
  let parsed;
  try {
    parsed = JSON.parse(raw);
  } catch (err) {
    throw new Error(`invalid JSON in ui-preview config at ${full}: ${err.message}`);
  }
  return checkConfig(parsed, full);
}

/** Lay `config` over the defaults and refuse what cannot work. */
export function checkConfig(config, where = "config") {
  if (typeof config !== "object" || config === null || Array.isArray(config)) {
    throw new Error(`${where}: the top-level config must be an object`);
  }
  const merged = { ...DEFAULTS, ...config };
  merged.auth = { ...DEFAULTS.auth, ...(config.auth ?? {}) };

  if (typeof merged.serve !== "string" || !merged.serve.trim()) {
    throw new Error(`${where}: "serve" must be the command that serves a checkout`);
  }
  if (typeof merged.ready !== "string" || !merged.ready.startsWith("/")) {
    throw new Error(`${where}: "ready" must be a path on the served app`);
  }
  if (
    !Array.isArray(merged.viewport) ||
    merged.viewport.length !== 2 ||
    !merged.viewport.every((n) => Number.isInteger(n) && n >= 320 && n <= 3840)
  ) {
    throw new Error(`${where}: "viewport" must be [width, height] in pixels`);
  }
  if (!Array.isArray(merged.entry_points)) {
    throw new Error(`${where}: "entry_points" must be a list of {name, path}`);
  }
  for (const entry of merged.entry_points) {
    if (typeof entry?.name !== "string" || typeof entry?.path !== "string" || !entry.path.startsWith("/")) {
      throw new Error(`${where}: every entry point needs a "name" and a "path" starting with /`);
    }
  }
  if (!Array.isArray(merged.mocks)) {
    throw new Error(`${where}: "mocks" must be a list`);
  }
  for (const mock of merged.mocks) {
    if (typeof mock?.url !== "string") {
      throw new Error(`${where}: every mock needs a "url" glob`);
    }
    if (!mock.dir && mock.body === undefined) {
      throw new Error(`${where}: mock ${mock.url} needs a "dir" of fixtures or an inline "body"`);
    }
  }
  if (!Array.isArray(merged.auth.cookies)) {
    throw new Error(`${where}: "auth.cookies" must be a list`);
  }
  for (const cookie of merged.auth.cookies) {
    if (typeof cookie?.name !== "string" || typeof cookie?.value !== "string") {
      throw new Error(`${where}: every auth cookie needs a "name" and a "value"`);
    }
  }
  if (!Number.isFinite(merged.timeout_s) || merged.timeout_s <= 0) {
    throw new Error(`${where}: "timeout_s" must be a positive finite number of seconds`);
  }
  if (!Array.isArray(merged.mask)) {
    throw new Error(`${where}: "mask" must be a list`);
  }
  if (!Number.isInteger(merged.max_flows) || merged.max_flows < 1) {
    throw new Error(`${where}: "max_flows" must be a positive integer`);
  }
  return merged;
}
