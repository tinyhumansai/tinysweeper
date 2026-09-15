// Executing the server's commands against a Playwright page.
//
// One arm per `op`, mirroring `src/preview/types.rs::Command` one to one;
// the Rust side pins the wire spelling in a test and this is the other half
// of that contract. Locators are Playwright's user-facing kinds only — the
// server never sends CSS — and every locator is `.first()`, because a name
// that matches twice is the model's problem to disambiguate on the next
// turn, not a reason to fail the batch.
//
// A batch stops at its first failure. Later commands almost always depend on
// earlier ones (click the tab, then screenshot the tab), so running on after
// a miss produces a screenshot of the wrong thing with the right name.
//
// After every batch the page is observed: its URL and its accessibility
// snapshot. That snapshot is what the server reads instead of pixels.

const COMMAND_TIMEOUT_MS = 10_000;
const MAX_WAIT_MS = 5_000;

/**
 * The per-command timeout for a replay on the base build.
 *
 * Shorter than the head's, because a miss there is the expected answer for
 * anything the pull request added, and waiting the full ten seconds to learn
 * what the plan already predicted is a minute per flow for nothing.
 */
export const REPLAY_TIMEOUT_MS = 4_000;

/** A Playwright locator for a server locator. */
export function resolveLocator(page, locator) {
  switch (locator?.by) {
    case "role":
      return page.getByRole(locator.role, {
        ...(locator.name ? { name: locator.name, exact: Boolean(locator.exact) } : {}),
      });
    case "label":
      return page.getByLabel(locator.text);
    case "text":
      return page.getByText(locator.text, { exact: Boolean(locator.exact) });
    case "placeholder":
      return page.getByPlaceholder(locator.text);
    case "test_id":
      return page.getByTestId(locator.id);
    default:
      throw new Error(`unknown locator kind ${JSON.stringify(locator?.by)}`);
  }
}

/**
 * Run `commands` on `page`.
 *
 * `ctx` carries: `origin`, the served app; `shots`, a Map the screenshots
 * are written into by id as `{png, url, step, callouts}`; `recorder`, with
 * `start()` and `stop()`; `masks`, selectors hidden in screenshots; `scale`,
 * the device scale factor; and `step`, the running count.
 */
export async function execute(page, commands, ctx) {
  const results = [];
  for (const [index, command] of commands.entries()) {
    try {
      await run(page, command, ctx);
      results.push({ index, ok: true });
    } catch (err) {
      results.push({ index, ok: false, error: String(err.message ?? err).slice(0, 400) });
      break;
    } finally {
      if (command.op !== "record" && command.op !== "done") {
        ctx.step += 1;
      }
    }
  }
  return results;
}

async function run(page, command, ctx) {
  const timeout = ctx.timeoutMs ?? COMMAND_TIMEOUT_MS;
  const t = { timeout };
  switch (command.op) {
    case "goto": {
      if (!command.path.startsWith("/") || command.path.startsWith("//")) {
        throw new Error("goto path must be same-origin");
      }
      await page.goto(`${ctx.origin}${command.path}`, { waitUntil: "load", timeout: 30_000 });
      await settle(page);
      return;
    }
    case "click":
      await resolveLocator(page, command.locator).first().click(t);
      await settle(page);
      return;
    case "fill":
      await resolveLocator(page, command.locator).first().fill(command.value, t);
      return;
    case "press":
      await page.keyboard.press(command.key);
      await settle(page);
      return;
    case "select":
      await resolveLocator(page, command.locator).first().selectOption(command.value, t);
      await settle(page);
      return;
    case "hover":
      await resolveLocator(page, command.locator).first().hover(t);
      return;
    case "wait":
      if (command.locator) {
        await resolveLocator(page, command.locator)
          .first()
          .waitFor({ state: "visible", timeout: Math.min(command.ms ?? timeout, timeout) });
      } else {
        await page.waitForTimeout(Math.min(command.ms ?? 500, MAX_WAIT_MS));
      }
      return;
    case "screenshot": {
      const mask = ctx.masks.map((selector) => page.locator(selector));
      const png = await page.screenshot({ fullPage: true, mask, animations: "disabled" });
      ctx.shots.set(command.id, {
        png,
        url: page.url(),
        step: ctx.step,
        callouts: [],
      });
      return;
    }
    case "annotate": {
      const shot = ctx.shots.get(command.shot);
      if (!shot) throw new Error(`no screenshot ${command.shot}`);
      // Boxes are measured now, on the page the screenshot was taken of a
      // moment ago; a full-page screenshot's origin is the document, so the
      // scroll offset is added back.
      const scrollY = await page.evaluate(() => window.scrollY);
      const scrollX = await page.evaluate(() => window.scrollX);
      // All or nothing: measured into a list first and appended only once
      // every target resolved, so a miss on the third callout does not
      // leave the first two on the shot for a later recovery to duplicate.
      const measured = [];
      let n = shot.callouts.length;
      for (const callout of command.callouts) {
        const box = await resolveLocator(page, callout.locator).first().boundingBox({ timeout });
        if (!box) throw new Error(`callout target is not visible: ${callout.label}`);
        n += 1;
        measured.push({
          n,
          label: callout.label,
          box: {
            x: (box.x + scrollX) * ctx.scale,
            y: (box.y + scrollY) * ctx.scale,
            w: box.width * ctx.scale,
            h: box.height * ctx.scale,
          },
        });
      }
      shot.callouts.push(...measured);
      return;
    }
    case "record":
      if (command.start) ctx.recorder.start();
      else ctx.recorder.stop();
      return;
    case "done":
      return;
    default:
      throw new Error(`unknown command ${JSON.stringify(command.op)}`);
  }
}

/** Let the page settle after an action, without hanging on a busy one. */
async function settle(page) {
  try {
    await page.waitForLoadState("networkidle", { timeout: 4_000 });
  } catch {
    // A page with a long-poll never goes idle; the snapshot is taken anyway.
  }
}

/** What the server is shown after a batch. */
export async function observe(page, { side, results, step }) {
  let aria = "";
  try {
    aria = await page.locator("body").ariaSnapshot({ timeout: COMMAND_TIMEOUT_MS });
  } catch (err) {
    aria = `(snapshot failed: ${err.message})`;
  }
  return { side, url: page.url(), aria, results, steps: step };
}
