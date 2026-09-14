import { test } from "node:test";
import assert from "node:assert/strict";

import { buildManifest, jobSummary } from "../src/manifest.mjs";

const flows = [
  {
    id: "f1",
    title: "Toggle the setting",
    status: "before_failed",
    failedAt: 3,
    clip: { video: "clip-01.mp4", gif: "clip-01.gif" },
    changes: [
      { n: 1, step: 4, path: "/settings", full: "change-01.png", crop: "change-01.crop.png", before: "before-01.png", callouts: [{ n: 1, label: "New toggle", box: { x: 1, y: 1, w: 1, h: 1 } }] },
    ],
  },
  { id: "f2", title: "Broken", status: "failed", failedAt: 1, clip: null, changes: [] },
];

test("the manifest is the wire shape the server validates, with no boxes and no URLs", () => {
  const m = buildManifest({ repo: "o/r", pullRequest: 7, headSha: "abc", baseSha: "b", run: "run-1", flows });
  assert.equal(m.version, 1);
  assert.deepEqual(m.flows[0].clip, { video: "clip-01.mp4", gif: "clip-01.gif" });
  assert.equal(m.flows[0].failed_at, 3);
  assert.deepEqual(m.flows[0].changes[0].callouts, [{ n: 1, label: "New toggle" }]);
  assert.equal(m.flows[0].changes[0].before, "before-01.png");
  assert.ok(!("clip" in m.flows[1]));
  assert.ok(!JSON.stringify(m).includes("http"));
});

test("the job summary is the same gallery and skips failed flows", () => {
  const m = buildManifest({ repo: "o/r", pullRequest: 7, headSha: "abc", baseSha: "b", run: "run-1", flows });
  const md = jobSummary(m, "https://p.example/");
  assert.ok(md.includes('<img src="https://p.example/o/r/abc/run-1/clip-01.gif" width="380"'));
  assert.ok(md.includes("new in this PR"));
  assert.ok(!md.includes("Broken"));
});

test("without a base url the summary names the files instead of linking", () => {
  const m = buildManifest({ repo: "o/r", pullRequest: 7, headSha: "abc", baseSha: "b", run: "run-1", flows });
  const md = jobSummary(m, null);
  assert.ok(md.includes("<code>change-01.crop.png</code>"));
  assert.ok(!md.includes("<img"));
});
