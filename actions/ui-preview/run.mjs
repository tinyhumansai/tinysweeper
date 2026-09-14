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
import { execute, observe } from "./src/driver.mjs";
import { annotate } from "./src/annotate.mjs";
import { recorder, convert } from "./src/clip.mjs";
import { upload } from "./src/upload.mjs";
import { buildManifest, jobSummary } from "./src/manifest.mjs";

const SCALE = 2;
const BEFORE_PORT = 3000;
const AFTER_PORT = 3001;

async function main() {
  const opts = options();
  const config = await loadConfig(opts.afterDir, opts.config);
  await mkdir(opts.out, { recursive: true });
  const log = (line) => console.error(line);

  // Both builds at once: they are independent and the runner minutes are
  // the bill. A `serve` script that cannot share a machine is a bug in it.
  const timeoutMs = config.timeout_s * 1000;
  const [after, before] = await Promise.all([
    serve({ command: config.serve, checkout: opts.afterDir, port: AFTER_PORT, ready: config.ready, timeoutMs, log }),
    serve({ command: config.serve, checkout: opts.beforeDir, port: BEFORE_PORT, ready: config.ready, timeoutMs, log }),
  ]);

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
    log(`[preview] session ${session.id}: ${started.flows.length} flow(s)`);
    for (const flow of started.flows) log(`[preview]   ${flow.id}: ${flow.title}`);

    const browser = await chromium.launch();
    const run = `run-${new Date().toISOString().replace(/[:.]/g, "-")}`;
    const results = [];
    let changeNumber = 0;
    try {
      for (const [i, flow] of started.flows.entries()) {
        log(`[preview] ${flow.id}: driving the head build`);
        const head = await drive({ browser, config, origin: after.origin, session, flow, side: "after", maxSteps: started.max_steps, out: opts.out, log });
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
          base = await replay({ browser, config, origin: before.origin, flow, script: head.script, out: opts.out, log });
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
          const mp4 = path.join(opts.out, `clip-${cc}.mp4`);
          const gif = path.join(opts.out, `clip-${cc}.gif`);
          try {
            await convert(head.video, head.span, { mp4, gif });
            result.clip = { mp4: `clip-${cc}.mp4`, gif: `clip-${cc}.gif` };
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

    if (opts.upload) {
      await upload({
        dir: opts.out,
        prefix: `${opts.repo}/${opts.headSha}/${run}`,
        bucket: opts.s3.bucket,
        endpoint: opts.s3.endpoint,
        region: opts.s3.region,
        log,
      });
    } else {
      log("[preview] --no-upload: the run stays in " + opts.out);
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

/** Drive one flow on the head build, asking the server each turn. */
async function drive({ browser, config, origin, session, flow, side, maxSteps, out, log }) {
  const context = await newContext({ browser, config, origin, out, flow, side });
  const epoch = Date.now();
  const rec = recorder(epoch);
  const page = await context.newPage();
  const ctx = { origin, shots: new Map(), recorder: rec, masks: config.mask, scale: SCALE, step: 0 };
  const script = [];
  let failedAt = null;

  try {
    await page.goto(`${origin}${flow.start_path}`, { waitUntil: "load", timeout: 30_000 });
    let results = [];
    for (let turn = 0; turn < maxSteps + 5; turn += 1) {
      const observation = await observe(page, { side, results, step: ctx.step });
      const reply = await session.step(flow.id, observation);
      results = await execute(page, reply.commands, ctx);
      script.push(...reply.commands.slice(0, results.length));
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
async function replay({ browser, config, origin, flow, script, out, log }) {
  const context = await newContext({ browser, config, origin, out, flow, side: "before", video: false });
  const page = await context.newPage();
  const ctx = { origin, shots: new Map(), recorder: recorder(), masks: config.mask, scale: SCALE, step: 0 };
  let failedAt = null;
  try {
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

/** A context with the repository's auth and mocks applied. */
async function newContext({ browser, config, origin, out, flow, side, video = true }) {
  const [width, height] = config.viewport;
  const context = await browser.newContext({
    viewport: { width, height },
    deviceScaleFactor: SCALE,
    baseURL: origin,
    ...(video ? { recordVideo: { dir: path.join(out, "video", `${flow.id}-${side}`), size: { width, height } } } : {}),
  });
  const host = new URL(origin).hostname;
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
      const answer = await fixtureFor(mock, route.request(), path.dirname(path.resolve(out)));
      if (answer) await route.fulfill(answer);
      else await route.fallback();
    });
  }
  return context;
}

/** The canned answer for a mocked request, if there is one. */
async function fixtureFor(mock, request, _root) {
  if (mock.body !== undefined) {
    return {
      status: mock.status ?? 200,
      contentType: mock.content_type ?? "application/json",
      body: typeof mock.body === "string" ? mock.body : JSON.stringify(mock.body),
    };
  }
  const { pathname } = new URL(request.url());
  const { readFile } = await import("node:fs/promises");
  const candidates = [
    path.join(mock.dir, `${pathname}.${request.method()}.json`),
    path.join(mock.dir, `${pathname}.json`),
    path.join(mock.dir, pathname, "index.json"),
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
  const body = manifest.flows
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
  if (opts.upload && (!opts.s3.bucket || !opts.s3.endpoint)) {
    throw new Error("uploading needs TS_S3_BUCKET and TS_S3_ENDPOINT, or pass --no-upload");
  }
  return opts;
}

main().catch((err) => {
  console.error(`[preview] ${err.stack ?? err}`);
  process.exitCode = 1;
});
