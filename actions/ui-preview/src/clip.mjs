// Clips: from Playwright's per-context video to an mp4 and a gif.
//
// Playwright records a context from the moment it opens; the server's
// `record start` and `record stop` are marks inside that recording. The
// clip is the span between them, cut with ffmpeg once the context is closed
// (closing is what flushes the file). The gif is the thumbnail — GitHub
// animates it inline — and links to the mp4, which is a tenth of the size
// for the same seconds.

import { execFile } from "node:child_process";
import { existsSync, readdirSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import { promisify } from "node:util";

const run = promisify(execFile);

/**
 * The ffmpeg to use: the one on PATH, else the one Playwright downloads for
 * its own video recording. The runner never needs an apt install for this.
 */
export function ffmpegBinary() {
  if (process.env.TS_FFMPEG) return process.env.TS_FFMPEG;
  for (const dir of process.env.PATH?.split(path.delimiter) ?? []) {
    const candidate = path.join(dir, "ffmpeg");
    if (existsSync(candidate)) return candidate;
  }
  const cache = process.env.PLAYWRIGHT_BROWSERS_PATH ?? path.join(os.homedir(), ".cache", "ms-playwright");
  try {
    const found = readdirSync(cache)
      .filter((d) => d.startsWith("ffmpeg-"))
      .sort()
      .reverse()
      .map((d) => path.join(cache, d, os.platform() === "win32" ? "ffmpeg-win64.exe" : os.platform() === "darwin" ? "ffmpeg-mac" : "ffmpeg-linux"))
      .find((f) => existsSync(f));
    if (found) return found;
  } catch {
    // No cache directory.
  }
  return "ffmpeg";
}

/** The longest a clip may be, in seconds. */
export const MAX_CLIP_S = 12;

/** A recorder that turns the two marks into a span relative to `epoch`. */
export function recorder(epoch = Date.now()) {
  const marks = { start: null, stop: null };
  return {
    start() {
      if (marks.start === null) marks.start = Date.now() - epoch;
    },
    stop() {
      if (marks.start !== null && marks.stop === null) marks.stop = Date.now() - epoch;
    },
    span() {
      if (marks.start === null) return null;
      const start = marks.start / 1000;
      const stop = (marks.stop ?? Date.now() - epoch) / 1000;
      return { start, stop: Math.min(stop, start + MAX_CLIP_S) };
    },
  };
}

/** Cut `webm` to `mp4` and `gif` over `span`. */
export async function convert(webm, span, { mp4, gif }) {
  const common = ["-y", "-loglevel", "error", "-ss", span.start.toFixed(2), "-to", span.stop.toFixed(2), "-i", webm];
  const ffmpeg = ffmpegBinary();
  await run(ffmpeg, [
    ...common,
    "-vf",
    "scale=1280:-2",
    "-c:v",
    "libx264",
    "-preset",
    "veryfast",
    "-crf",
    "23",
    "-pix_fmt",
    "yuv420p",
    "-movflags",
    "+faststart",
    "-an",
    mp4,
  ]);
  await run(ffmpeg, [
    ...common,
    "-vf",
    "fps=10,scale=600:-1:flags=lanczos,split[a][b];[a]palettegen=max_colors=128[p];[b][p]paletteuse=dither=bayer:bayer_scale=5",
    "-loop",
    "0",
    gif,
  ]);
}
