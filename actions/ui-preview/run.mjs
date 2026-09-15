#!/usr/bin/env node
// The hands of tinysweeper's UI preview, end to end.
//
//   1. Serve the merge-base and the head, each from the repository's own
//      `serve` script.
//   2. Open a session with the server; it plans the flows from the diff.
//   3. For each flow, on the head: ask the server for commands, run them,
//      show it the page, repeat until it says done. Then replay the same
//      script on the merge-base, so every screenshot has a "before".
//   4. Draw the callouts, cut the clips, write the manifest, upload the run.
//   5. Hand the manifest to the server, which publishes the comment.
//
// The server decides everything; this file only executes and reports. It
// holds the bucket credential and the server token and nothing else.
//
// Usage in CI is through action.yml. Locally:
//
//   TS_TOKEN=… node run.mjs --server http://localhost:8081 \
//     --before ../before --after . --repo o/r --pr 7 \
//     --head-sha $(git rev-parse HEAD) --base-sha $(git merge-base HEAD main) \
//     --no-upload

import { mkdir, writeFile, appendFile } from "node:fs/promises";
import path from "node:path";
import { chromium } from "playwright";

import { loadConfig } from "./src/config.mjs";
import { serve } from "./src/serve.mjs";
import { Session } from "./src/session.mjs";
import { execute, observe, REPLAY_TIMEOUT_MS } from "./src/driver.mjs";
import { annotate } from "./src/annotate.mjs";
import { recorder, convert } from "./src/clip.mjs";
import { upload, uploadToServer } from "./src/upload.mjs";
import { buildManifest, jobSummary } from "./src/manifest.mjs";

const SCALE = 2;
// Overridable for a developer machine where something already listens on
// the defaults; on a CI runner nothing does.
const BEFORE_PORT = Number(process.env.TS_BEFORE_PORT ?? 3000);
const AFTER_PORT = Number(process.env.TS_AFTER_PORT ?? 3001);

async function main() {
  const opts = options();
  const config = await loadConfig(opts.afterDir, opts.config);
  await mkdir(opts.out, { recursive: true });
  const log = (line) => console.error(line);

  // Both builds at once: they are independent and the runner minutes are
  // the bill. A `serve` script that cannot share a machine is a bug in it.
  const timeoutMs = config.timeout_s * 1000;
  const { after, before } = await serveBoth({
    command: config.serve,
    afterDir: opts.afterDir,
    beforeDir: opts.beforeDir,
    afterPort: AFTER_PORT,
    beforePort: BEFORE_PORT,
    ready: config.ready,
    timeoutMs,
    log,
  });

  let exitCode = 0;
  try {
    const session = new Session({ server: opts.server, token: opts.token });
    const started = await session.start({
      repo: opts.repo,
      pull_request: opts.pr,
      head_sha: opts.headSha,
      base_sha: opts.baseSha,
      entry_points: config.entry_points.map((e) => [e.name, e.path]),
    });
    if (!started.enabled) {
      log("[preview] previews are off for this repository; nothing to do");
      return;
    }
    if (started.flows.length === 0) {
      log("[preview] the server planned no flows: the diff changes nothing a user can see");
      await summary(opts, { flows: [] });
      return;
    }
    // The server's plan can exceed the repository's own cap: `max_flows` in
    // `.tinysweeper/ui-preview.json` is documented as a hard ceiling on the
    // browser work and model turns this run performs, so enforce it here
    // even if the server already tried to.
    const flows = started.flows.slice(0, config.max_flows);
    log(`[preview] session ${session.id}: ${flows.length} flow(s)`);
    for (const flow of flows) log(`[preview]   ${flow.id}: ${flow.title}`);

    // `--disable-dev-shm-usage`: Chromium's frame capture allocates in
    // /dev/shm, which a container gives 64 MB and some hosts quota; without
    // this a 2x full-page screenshot dies with "Target crashed".
    const browser = await chromium.launch({ args: ["--disable-dev-shm-usage"] });
    const run = `run-${new Date().toISOString().replace(/[:.]/g, "-")}`;
    const results = [];
    let changeNumber = 0;
    try {
      for (const [i, flow] of flows.entries()) {
        log(`[preview] ${flow.id}: driving the head build`);
        const head = await drive({ browser, config, origin: after.origin, checkoutDir: opts.afterDir, session, flow, side: "after", maxSteps: started.max_steps, out: opts.out, log });
        const result = { id: flow.id, title: flow.title, status: "ok", changes: [], clip: null };
        if (head.failedAt !== null) {
          result.status = "failed";
          result.failedAt = head.failedAt;
        }

        // The base build replays the same script. It asks the server
        // nothing; a step that fails there is the evidence, not an error.
        let base = null;
        if (result.status === "ok" && head.script.length > 0) {
          log(`[preview] ${flow.id}: replaying on the base build`);
          base = await replay({ browser, config, origin: before.origin, checkoutDir: opts.beforeDir, flow, script: head.script, out: opts.out, log });
          if (base.failedAt !== null) {
            result.status = "before_failed";
            result.failedAt = base.failedAt;
          }
        }

        // Pictures: every head screenshot with callouts becomes a change;
        // the base screenshot with the same id rides beside it.
        for (const [id, shot] of head.shots) {
          if (shot.callouts.length === 0) continue;
          changeNumber += 1;
          const nn = String(changeNumber).padStart(2, "0");
          const { full, crop } = await annotate(shot.png, shot.callouts, { scale: SCALE });
          await writeFile(path.join(opts.out, `change-${nn}.png`), full);
          await writeFile(path.join(opts.out, `change-${nn}.crop.png`), crop);
          const change = {
            n: result.changes.length + 1,
            step: shot.step,
            path: pathOf(shot.url),
            full: `change-${nn}.png`,
            crop: `change-${nn}.crop.png`,
            callouts: shot.callouts,
          };
          const beforeShot = base?.shots.get(id);
          if (beforeShot) {
            await writeFile(path.join(opts.out, `before-${nn}.png`), beforeShot.png);
            change.before = `before-${nn}.png`;
          }
          result.changes.push(change);
        }

        if (head.video && head.span) {
          const cc = String(i + 1).padStart(2, "0");
          try {
            const { video } = await convert(head.video, head.span, path.join(opts.out, `clip-${cc}`));
            result.clip = { video: path.basename(video), gif: `clip-${cc}.gif` };
          } catch (err) {
            log(`[preview] ${flow.id}: clip conversion failed: ${err.message}`);
          }
        }
        results.push(result);
      }
    } finally {
      await browser.close();
    }

    const manifest = buildManifest({
      repo: opts.repo,
      pullRequest: opts.pr,
      headSha: opts.headSha,
      baseSha: opts.baseSha,
      run,
      flows: results,
    });
    await writeFile(path.join(opts.out, "manifest.json"), JSON.stringify(manifest, null, 2));

    if (!opts.upload) {
      log("[preview] --no-upload: the run stays in " + opts.out);
    } else if (opts.s3.bucket) {
      await upload({
        dir: opts.out,
        prefix: `${opts.repo}/${opts.headSha}/${run}`,
        bucket: opts.s3.bucket,
        endpoint: opts.s3.endpoint,
        region: opts.s3.region,
        log,
      });
    } else {
      // The default: the server keeps the files until `finish` and commits
      // them to the repository's store branch itself.
      await uploadToServer({ dir: opts.out, session, log });
    }
    await summary(opts, manifest);

    const finished = await session.finish(manifest);
    log(`[preview] published: ${finished.outcome}`);
  } catch (err) {
    console.error(`[preview] failed: ${err.stack ?? err}`);
    exitCode = 1;
  } finally {
    after.stop();
    before.stop();
  }
  process.exitCode = exitCode;
}

/**
 * Start both servers concurrently. `Promise.all` would reject as soon as one
 * side fails and leave the other detached — its child process outlives the
 * job with open stdio pipes, so Node never exits. Settle both, then stop
 * whichever one survives before propagating the failure.
 */
async function serveBoth({ command, afterDir, beforeDir, afterPort, beforePort, ready, timeoutMs, log }) {
  const [afterResult, beforeResult] = await Promise.allSettled([
    serve({ command, checkout: afterDir, port: afterPort, ready, timeoutMs, log }),
    serve({ command, checkout: beforeDir, port: beforePort, ready, timeoutMs, log }),
  ]);
  if (afterResult.status === "rejected" || beforeResult.status === "rejected") {
    if (afterResult.status === "fulfilled") afterResult.value.stop();
    if (beforeResult.status === "fulfilled") beforeResult.value.stop();
    const failure = afterResult.status === "rejected" ? afterResult.reason : beforeResult.reason;
    throw failure;
  }
  return { after: afterResult.value, before: beforeResult.value };
}

/** Drive one flow on the head build, asking the server each turn. */
async function drive({ browser, config, origin, checkoutDir, session, flow, side, maxSteps, out, log }) {
  const context = await newContext({ browser, config, origin, checkoutDir, out, flow, side });
  const epoch = Date.now();
  const rec = recorder(epoch);
  const page = await context.newPage();
  const ctx = { origin, shots: new Map(), recorder: rec, masks: config.mask, scale: SCALE, step: 0 };
  const script = [];
  let failedAt = null;

  try {
    await bootstrap(page, config, origin);
    await page.goto(`${origin}${flow.start_path}`, { waitUntil: "load", timeout: 30_000 });
    let results = [];
    for (let turn = 0; turn < maxSteps + 5; turn += 1) {
      const observation = await observe(page, { side, results, step: ctx.step });
      const reply = await session.step(flow.id, observation);
      // Every turn's snapshot and answer, for reading afterwards why a flow
      // went where it went. Under the run directory, so the artifact keeps it.
      await writeFile(
        path.join(out, `turn-${flow.id}-${String(turn).padStart(2, "0")}.json`),
        JSON.stringify({ observation, reply }, null, 2),
      );
      results = await execute(page, reply.commands, ctx);
      // `execute` stops at the first failing command, so at most the last
      // entry in `results` is a failure. The base build's `replay` later
      // runs the whole recorded `script` in one `execute` call too — if a
      // failed command rode along, replay would stop there and never reach
      // whatever the model recovered with in a later turn, misreporting
      // where the base build actually diverges. Record only what succeeded.
      const failedIndex = results.findIndex((r) => !r.ok);
      const succeeded = failedIndex === -1 ? results.length : failedIndex;
      script.push(...reply.commands.slice(0, succeeded));
      const failed = results.find((r) => !r.ok);
      if (failed) {
        log(`[preview] ${flow.id}: step ${ctx.step} failed: ${failed.error}`);
      }
      if (reply.done) break;
      if (ctx.step >= maxSteps) break;
    }
    // A flow whose last batch failed before `done` did not reach its goal.
    const last = results[results.length - 1];
    if (last && !last.ok) failedAt = ctx.step;
  } finally {
    rec.stop();
    await page.close();
    await context.close();
  }
  const video = await videoPath(page);
  return { shots: ctx.shots, script, failedAt, video, span: rec.span() };
}

/** Replay a script on the base build; no server, no callouts.*/
async function replay({ browser, config, origin, checkoutDir, flow, script, out, log }) {
  const context = await newContext({ browser, config, origin, checkoutDir, out, flow, side: "before", video: false });
  const page = await context.newPage();
  const ctx = { origin, shots: new Map(), recorder: recorder(), masks: config.mask, scale: SCALE, step: 0, timeoutMs: REPLAY_TIMEOUT_MS };
  let failedAt = null;
  try {
    await bootstrap(page, config, origin);
    await page.goto(`${origin}${flow.start_path}`, { waitUntil: "load", timeout: 30_000 });
    const replayable = script.filter((c) => c.op !== "annotate" && c.op !== "record" && c.op !== "done");
    const results = await execute(page, replayable, ctx);
    const failed = results.find((r) => !r.ok);
    if (failed) {
      failedAt = failed.index + 1;
      log(`[preview] ${flow.id}: base build failed at step ${failedAt}: ${failed.error}`);
    }
  } finally {
    await page.close();
    await context.close();
  }
  return { shots: ctx.shots, failedAt };
}

/**
 * Open the repository's bootstrap page first, when it has one.
 *
 * `auth.visit` is for the case cookies and static localStorage cannot cover:
 * a value only the served side knows, such as the port of a backend the
 * `serve` script started. The page seeds whatever it needs and redirects;
 * openhuman's dev server has the same `/__dev-connect` route.
 */
async function bootstrap(page, config, origin) {
  if (!config.auth.visit) return;
  await page.goto(`${origin}${config.auth.visit}`, { waitUntil: "load", timeout: 30_000 });
  try {
    await page.waitForLoadState("networkidle", { timeout: 5_000 });
  } catch {
    // A redirect target that long-polls never goes idle.
  }
}

/** A context with the repository's auth and mocks applied. */
async function newContext({ browser, config, origin, checkoutDir, out, flow, side, video = true }) {
  const [width, height] = config.viewport;
  const context = await browser.newContext({
    viewport: { width, height },
    deviceScaleFactor: SCALE,
    baseURL: origin,
    ...(video ? { recordVideo: { dir: path.join(out, "video", `${flow.id}-${side}`), size: { width, height } } } : {}),
  });
  let host;
  try {
    host = new URL(origin).hostname;
  } catch (err) {
    throw new Error(`invalid served origin ${JSON.stringify(origin)}: ${err.message}`);
  }
  if (config.auth.cookies.length > 0) {
    await context.addCookies(
      config.auth.cookies.map((c) => ({ domain: host, path: "/", ...c })),
    );
  }
  const storage = config.auth.localStorage ?? {};
  if (Object.keys(storage).length > 0) {
    await context.addInitScript((items) => {
      for (const [k, v] of Object.entries(items)) {
        if (window.localStorage.getItem(k) === null) window.localStorage.setItem(k, v);
      }
    }, storage);
  }
  for (const mock of config.mocks) {
    await context.route(mock.url, async (route) => {
      const answer = await fixtureFor(mock, route.request(), checkoutDir);
      if (answer) await route.fulfill(answer);
      else await route.fallback();
    });
  }
  return context;
}

/** The canned answer for a mocked request, if there is one. */
async function fixtureFor(mock, request, checkoutDir) {
  if (mock.body !== undefined) {
    return {
      status: mock.status ?? 200,
      contentType: mock.content_type ?? "application/json",
      body: typeof mock.body === "string" ? mock.body : JSON.stringify(mock.body),
    };
  }
  const { pathname } = new URL(request.url());
  const { readFile } = await import("node:fs/promises");
  // `mock.dir` is relative to the config file (repository root), and
  // `checkoutDir` is the side-specific checkout (before or after) driving
  // this context — resolving against it, not the shared output directory,
  // keeps each side's fixtures separate.
  const dir = path.resolve(checkoutDir, mock.dir);
  const candidates = [
    path.join(dir, `${pathname}.${request.method()}.json`),
    path.join(dir, `${pathname}.json`),
    path.join(dir, pathname, "index.json"),
  ];
  for (const candidate of candidates) {
    try {
      const body = await readFile(candidate, "utf8");
      return { status: 200, contentType: "application/json", body };
    } catch {
      // Try the next spelling.
    }
  }
  return null;
}

async function videoPath(page) {
  try {
    return await page.video()?.path();
  } catch {
    return null;
  }
}

function pathOf(url) {
  try {
    const u = new URL(url);
    return `${u.pathname}${u.search}`;
  } catch {
    return url;
  }
}

/** The job summary, and the manifest beside it for the artifact. */
async function summary(opts, manifest) {
  if (!process.env.GITHUB_STEP_SUMMARY) return;
  const body = manifest.flows?.length
    ? jobSummary(manifest, opts.publicBaseUrl)
    : "### 🎬 UI preview\n\n_No user flow to show._\n";
  await appendFile(process.env.GITHUB_STEP_SUMMARY, body);
}

function options() {
  const args = process.argv.slice(2);
  const get = (flag, env) => {
    const i = args.indexOf(flag);
    return i >= 0 ? args[i + 1] : process.env[env];
  };
  const opts = {
    server: get("--server", "TS_SERVER"),
    token: process.env.TS_TOKEN,
    config: get("--config", "TS_CONFIG") ?? ".tinysweeper/ui-preview.json",
    beforeDir: path.resolve(get("--before", "TS_BEFORE_DIR") ?? "../before"),
    afterDir: path.resolve(get("--after", "TS_AFTER_DIR") ?? "."),
    repo: get("--repo", "TS_REPO"),
    pr: Number(get("--pr", "TS_PR")),
    headSha: get("--head-sha", "TS_HEAD_SHA"),
    baseSha: get("--base-sha", "TS_BASE_SHA"),
    out: path.resolve(get("--out", "TS_OUT") ?? "ui-preview-out"),
    upload: !args.includes("--no-upload"),
    publicBaseUrl: get("--public-base-url", "TS_PUBLIC_BASE_URL") ?? null,
    s3: {
      bucket: process.env.TS_S3_BUCKET,
      endpoint: process.env.TS_S3_ENDPOINT,
      region: process.env.TS_S3_REGION ?? "auto",
    },
  };
  for (const key of ["server", "token", "repo", "headSha", "baseSha"]) {
    if (!opts[key]) throw new Error(`missing ${key}; set it as a flag or environment variable`);
  }
  if (!Number.isInteger(opts.pr) || opts.pr <= 0) throw new Error("missing or invalid pull request number");
  if (opts.s3.bucket && !opts.s3.endpoint) {
    throw new Error("TS_S3_BUCKET is set without TS_S3_ENDPOINT");
  }
  return opts;
}

main().catch((err) => {
  console.error(`[preview] ${err.stack ?? err}`);
  process.exitCode = 1;
});
