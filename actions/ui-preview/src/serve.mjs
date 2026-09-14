// Serving one checkout: spawn the repository's own script, wait for it.
//
// Build-and-serve is deliberately not generic. A Next.js app wants
// `next build && next start`; openhuman wants a Rust core, a mock backend and
// a Vite preview. The repository owns that in its `serve` script, and the
// runner only knows the contract: the script is started with `TS_CHECKOUT`
// and `TS_PORT` in its environment, and the app is ready when `ready`
// answers 200 on that port. Both sides are served at once — the two builds
// are independent, and CI minutes are the bill.

import { spawn } from "node:child_process";
import { setTimeout as sleep } from "node:timers/promises";

// The spawned `serve` command is the checked-out repository's own script —
// same-repository pull requests still run whatever is at
// `scripts/ui-preview/serve.sh` on that head, so it is not fully trusted.
// This action holds the server token and the object-store credentials
// (module doc above); neither belongs in an environment that repository code
// controls.
const SECRET_ENV_KEYS = ["TS_TOKEN", "TS_S3_BUCKET", "TS_S3_ENDPOINT", "AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_SESSION_TOKEN"];

/** Start `command` for `checkout` on `port`; resolve once `ready` answers. */
export async function serve({ command, checkout, port, ready, timeoutMs, log = console.error }) {
  const env = { ...process.env, TS_CHECKOUT: checkout, TS_PORT: String(port) };
  for (const key of SECRET_ENV_KEYS) delete env[key];
  const child = spawn(command, {
    shell: true,
    cwd: checkout,
    env,
    stdio: ["ignore", "pipe", "pipe"],
    detached: true,
  });
  const tag = `[serve :${port}]`;
  const tail = [];
  const keep = (chunk) => {
    for (const line of chunk.toString().split("\n")) {
      if (!line) continue;
      tail.push(line);
      if (tail.length > 40) tail.shift();
      log(`${tag} ${line}`);
    }
  };
  child.stdout.on("data", keep);
  child.stderr.on("data", keep);

  let exited = null;
  // `spawn` itself can fail after returning — the command does not exist, or
  // is not executable — and that only ever shows up as an `'error'` event,
  // never `'exit'`. Without a handler here the health-check loop below just
  // times out and reports a confusing "was not ready" instead of the real
  // launch failure.
  child.on("error", (err) => {
    exited ??= { code: null, signal: null, error: err };
  });
  child.on("exit", (code, signal) => {
    exited ??= { code, signal };
  });

  const origin = `http://127.0.0.1:${port}`;
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (exited) {
      throw new Error(
        `${tag} exited before ${origin}${ready} was ready (code ${exited.code}, signal ${exited.signal}). Last output:\n${tail.join("\n")}`,
      );
    }
    if (await answers(`${origin}${ready}`)) {
      return {
        origin,
        stop: () => stop(child),
      };
    }
    await sleep(1000);
  }
  stop(child);
  throw new Error(`${tag} ${origin}${ready} was not ready after ${timeoutMs / 1000}s. Last output:\n${tail.join("\n")}`);
}

async function answers(url) {
  try {
    const response = await fetch(url, { redirect: "manual", signal: AbortSignal.timeout(3000) });
    return response.status >= 200 && response.status < 400;
  } catch {
    return false;
  }
}

/** Stop the whole process group: the script started children of its own. */
function stop(child) {
  if (child.exitCode !== null) return;
  try {
    process.kill(-child.pid, "SIGTERM");
  } catch {
    try {
      child.kill("SIGTERM");
    } catch {
      // Already gone.
    }
  }
}
