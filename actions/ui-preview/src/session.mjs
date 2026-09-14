// The HTTP client for the server's three /preview routes.
//
// The token rides in a header and never in a URL or an argument, where a
// shell trace or a proxy log would keep it. A 5xx is retried a few times
// with a pause, because the server's own retry is the model's and a model
// hiccup should not cost the whole run; a 4xx is the runner's own mistake
// and is not retried, because the same request would get the same answer.

import { setTimeout as sleep } from "node:timers/promises";

const RETRIES = 3;
const TIMEOUT_MS = 120_000;

export class Session {
  constructor({ server, token, fetchImpl = fetch }) {
    this.server = server.replace(/\/+$/, "");
    this.token = token;
    this.fetch = fetchImpl;
    this.id = null;
  }

  /** Open a session; returns the server's reply. */
  async start(body) {
    const reply = await this.post("/preview/sessions", body);
    this.id = reply.session ?? null;
    return reply;
  }

  /** One turn of one flow. */
  async step(flowId, observation) {
    return this.post(`/preview/sessions/${this.id}/flows/${flowId}/step`, observation);
  }

  /** Hand over the manifest. */
  async finish(manifest) {
    return this.post(`/preview/sessions/${this.id}/finish`, { manifest });
  }

  async post(path, body) {
    const url = `${this.server}${path}`;
    let last;
    for (let attempt = 1; attempt <= RETRIES; attempt += 1) {
      let response;
      try {
        response = await this.fetch(url, {
          method: "POST",
          headers: {
            authorization: `Bearer ${this.token}`,
            "content-type": "application/json",
            accept: "application/json",
          },
          body: JSON.stringify(body),
          signal: AbortSignal.timeout(TIMEOUT_MS),
        });
      } catch (err) {
        last = new Error(`${path}: ${err.message}`);
        await sleep(2000 * attempt);
        continue;
      }
      const text = await response.text();
      if (response.ok) {
        return text ? JSON.parse(text) : {};
      }
      const detail = errorDetail(text);
      if (response.status >= 500 && attempt < RETRIES) {
        last = new Error(`${path}: ${response.status} ${detail}`);
        await sleep(2000 * attempt);
        continue;
      }
      const err = new Error(`${path}: ${response.status} ${detail}`);
      err.status = response.status;
      throw err;
    }
    throw last;
  }
}

function errorDetail(text) {
  try {
    const parsed = JSON.parse(text);
    return parsed.error ?? text;
  } catch {
    return text.slice(0, 300);
  }
}
