import { test } from "node:test";
import assert from "node:assert/strict";

import { Session } from "../src/session.mjs";

function fakeFetch(answers) {
  const calls = [];
  const fetchImpl = async (url, init) => {
    calls.push({ url, init });
    const next = answers.shift();
    return {
      ok: next.status < 400,
      status: next.status,
      text: async () => JSON.stringify(next.body),
    };
  };
  return { fetchImpl, calls };
}

test("the token rides in the header and the session id in the path", async () => {
  const { fetchImpl, calls } = fakeFetch([
    { status: 200, body: { enabled: true, session: "s1", flows: [], max_steps: 25 } },
    { status: 200, body: { commands: [], done: true } },
  ]);
  const session = new Session({ server: "https://sweeper.example/", token: "tok", fetchImpl });
  await session.start({ repo: "o/r" });
  await session.step("f1", { side: "after" });
  assert.equal(calls[0].url, "https://sweeper.example/preview/sessions");
  assert.equal(calls[0].init.headers.authorization, "Bearer tok");
  assert.equal(calls[1].url, "https://sweeper.example/preview/sessions/s1/flows/f1/step");
  assert.ok(!calls[1].url.includes("tok"));
});

test("a 5xx is retried and a 4xx is not", async () => {
  const flaky = fakeFetch([
    { status: 503, body: { error: "model down" } },
    { status: 200, body: { enabled: false } },
  ]);
  const session = new Session({ server: "https://s.example", token: "t", fetchImpl: flaky.fetchImpl });
  const reply = await session.start({});
  assert.equal(reply.enabled, false);
  assert.equal(flaky.calls.length, 2);

  const mistaken = fakeFetch([{ status: 422, body: { error: "head moved" } }]);
  const other = new Session({ server: "https://s.example", token: "t", fetchImpl: mistaken.fetchImpl });
  await assert.rejects(other.start({}), /422 head moved/);
  assert.equal(mistaken.calls.length, 1);
});
