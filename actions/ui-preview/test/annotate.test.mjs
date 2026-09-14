import { test } from "node:test";
import assert from "node:assert/strict";
import sharp from "sharp";

import { placePill, overlaySvg, cropBox, annotate } from "../src/annotate.mjs";

const W = 2880;
const H = 1800;

test("a pill sits to the right of its element when there is room", () => {
  const p = placePill({ width: W, height: H, box: { x: 100, y: 100, w: 200, h: 40 }, label: "New toggle", scale: 2 });
  assert.equal(p.side, "right");
  assert.ok(p.x > 300, "starts right of the element");
  assert.ok(Math.abs(p.y + p.h / 2 - 120) < 1, "centred on the element");
  assert.equal(p.to.x, 300, "the leader ends on the element's right edge");
});

test("a pill near the right edge drops below its element and stays inside the image", () => {
  const p = placePill({ width: W, height: H, box: { x: 2700, y: 100, w: 150, h: 40 }, label: "A fairly long label here", scale: 2 });
  assert.equal(p.side, "below");
  assert.ok(p.x + p.w <= W, "inside on the right");
  assert.ok(p.y >= 140, "below the element");
});

test("a pill at the bottom right goes above its element", () => {
  const p = placePill({ width: W, height: H, box: { x: 2700, y: 1740, w: 150, h: 40 }, label: "Bottom", scale: 2 });
  assert.equal(p.side, "above");
  assert.ok(p.y + p.h <= 1740);
});

test("the overlay escapes labels so a hostile one cannot close the svg", () => {
  const svg = overlaySvg({
    width: W,
    height: H,
    scale: 2,
    callouts: [{ n: 1, label: '</text><script>x</script>"', box: { x: 10, y: 10, w: 10, h: 10 } }],
  });
  assert.ok(!svg.includes("<script>"));
  assert.ok(svg.includes("&lt;/text&gt;&lt;script&gt;"));
  assert.ok(svg.includes(">1</text>"), "the number is drawn");
});

test("the crop covers every callout and its pill, padded, at a readable minimum", () => {
  const callouts = [
    { n: 1, label: "One", box: { x: 400, y: 300, w: 100, h: 30 } },
    { n: 2, label: "Two", box: { x: 900, y: 700, w: 100, h: 30 } },
  ];
  const c = cropBox({ width: W, height: H, callouts, scale: 2 });
  assert.ok(c.left <= 400 - 96 && c.top <= 300 - 96, "padded past the first box");
  assert.ok(c.left + c.width >= 1000 + 96 && c.top + c.height >= 730 + 96, "past the second box");
  assert.ok(c.width >= 1800 && c.height >= 1120, "at least the 900x560 minimum at 2x");
  assert.ok(c.left >= 0 && c.top >= 0 && c.left + c.width <= W && c.top + c.height <= H, "inside the image");
});

test("no callouts crops to the whole image", () => {
  assert.deepEqual(cropBox({ width: W, height: H, callouts: [], scale: 2 }), { left: 0, top: 0, width: W, height: H });
});

test("annotate composites and crops a real png", async () => {
  const png = await sharp({ create: { width: 1200, height: 800, channels: 4, background: "#ffffff" } }).png().toBuffer();
  const { full, crop, region } = await annotate(png, [{ n: 1, label: "Here", box: { x: 100, y: 100, w: 80, h: 30 } }], { scale: 1 });
  const fullMeta = await sharp(full).metadata();
  assert.equal(fullMeta.width, 1200);
  const cropMeta = await sharp(crop).metadata();
  assert.equal(cropMeta.width, region.width);
  assert.equal(cropMeta.height, region.height);
  // The pill is indigo on white: the composited image is no longer all white.
  const { data } = await sharp(full).raw().toBuffer({ resolveWithObject: true });
  assert.ok(data.some((byte, i) => i % 4 === 0 && byte < 200), "something was drawn");
});
