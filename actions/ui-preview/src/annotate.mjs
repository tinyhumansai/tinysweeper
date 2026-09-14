// Numbered callouts on a screenshot, and the crop around them.
//
// The look is one pill per callout: an indigo circle with the number, a
// white rounded label beside it, and a thin leader to the element it names.
// Placed to the right of the element when there is room and below it when
// there is not, and always kept inside the image. Everything geometric is a
// pure function of the image size and the boxes, so it is tested without a
// browser; `sharp` only composites the SVG this file writes.
//
// `callout.box` arrives already scaled to device pixels: `driver.mjs`
// multiplies Playwright's CSS-pixel `boundingBox()` by the device scale
// factor before it ever reaches this module, to match the full-page
// screenshot taken at that same scale. Nothing here re-scales `box`; `scale`
// is only used for this module's own pill geometry (font, padding, stroke)
// so a pill drawn at 1x on a 2x image is not one nobody can read.

import sharp from "sharp";

const INDIGO = "#4f46e5";

/** Geometry of one pill, in image pixels. */
export function placePill({ width, height, box, label, scale }) {
  const s = scale;
  const font = 20 * s;
  const pad = 18 * s;
  const circle = 20 * s;
  const gap = 12 * s;
  const textWidth = Math.ceil(label.length * font * 0.56);
  const pillW = circle * 2 + gap + textWidth + pad * 1.5;
  const pillH = 44 * s;
  const margin = 12 * s;

  // Right of the element, vertically centred on it.
  let x = box.x + box.w + 24 * s;
  let y = box.y + box.h / 2 - pillH / 2;
  let side = "right";
  if (x + pillW + margin > width) {
    // Below it, left-aligned with its left edge.
    x = box.x;
    y = box.y + box.h + 16 * s;
    side = "below";
    if (y + pillH + margin > height) {
      // Above it.
      y = box.y - 16 * s - pillH;
      side = "above";
    }
  }
  x = Math.max(margin, Math.min(x, width - pillW - margin));
  y = Math.max(margin, Math.min(y, height - pillH - margin));

  // The leader runs from the pill's nearest edge to the element's nearest
  // edge; a line that crosses the element it points at is worse than none.
  const from =
    side === "right"
      ? { x, y: y + pillH / 2 }
      : side === "below"
        ? { x: x + circle, y }
        : { x: x + circle, y: y + pillH };
  const to =
    side === "right"
      ? { x: box.x + box.w, y: box.y + box.h / 2 }
      : side === "below"
        ? { x: box.x + Math.min(box.w / 2, circle * 2), y: box.y + box.h }
        : { x: box.x + Math.min(box.w / 2, circle * 2), y: box.y };

  return { x, y, w: pillW, h: pillH, font, circle, pad, gap, side, from, to };
}

/** The SVG overlay for every callout. */
export function overlaySvg({ width, height, callouts, scale }) {
  const parts = [];
  for (const callout of callouts) {
    const p = placePill({ width, height, box: callout.box, label: callout.label, scale });
    const stroke = 2.5 * scale;
    parts.push(
      `<line x1="${p.from.x}" y1="${p.from.y}" x2="${p.to.x}" y2="${p.to.y}" stroke="${INDIGO}" stroke-width="${stroke}" stroke-opacity="0.85"/>`,
      `<rect x="${p.x}" y="${p.y}" width="${p.w}" height="${p.h}" rx="${p.h / 2}" fill="#ffffff" stroke="${INDIGO}" stroke-width="${stroke}"/>`,
      `<circle cx="${p.x + p.circle + stroke}" cy="${p.y + p.h / 2}" r="${p.circle - stroke}" fill="${INDIGO}"/>`,
      `<text x="${p.x + p.circle + stroke}" y="${p.y + p.h / 2}" font-family="Inter, -apple-system, Segoe UI, Helvetica, Arial, sans-serif" font-size="${p.font * 0.9}" font-weight="600" fill="#ffffff" text-anchor="middle" dominant-baseline="central">${callout.n}</text>`,
      `<text x="${p.x + p.circle * 2 + p.gap}" y="${p.y + p.h / 2}" font-family="Inter, -apple-system, Segoe UI, Helvetica, Arial, sans-serif" font-size="${p.font}" fill="#111827" dominant-baseline="central">${escapeXml(callout.label)}</text>`,
    );
  }
  return `<svg xmlns="http://www.w3.org/2000/svg" width="${width}" height="${height}" viewBox="0 0 ${width} ${height}">${parts.join("")}</svg>`;
}

/**
 * The crop around every callout and its pill: their union, padded, grown to
 * a readable minimum, clamped to the image.
 */
export function cropBox({ width, height, callouts, scale, pad = 48, minW = 900, minH = 560 }) {
  let x0 = Infinity;
  let y0 = Infinity;
  let x1 = -Infinity;
  let y1 = -Infinity;
  for (const callout of callouts) {
    const p = placePill({ width, height, box: callout.box, label: callout.label, scale });
    for (const b of [callout.box, { x: p.x, y: p.y, w: p.w, h: p.h }]) {
      x0 = Math.min(x0, b.x);
      y0 = Math.min(y0, b.y);
      x1 = Math.max(x1, b.x + b.w);
      y1 = Math.max(y1, b.y + b.h);
    }
  }
  if (!Number.isFinite(x0)) {
    return { left: 0, top: 0, width, height };
  }
  x0 -= pad * scale;
  y0 -= pad * scale;
  x1 += pad * scale;
  y1 += pad * scale;
  const needW = Math.max(minW * scale, x1 - x0);
  const needH = Math.max(minH * scale, y1 - y0);
  const cx = (x0 + x1) / 2;
  const cy = (y0 + y1) / 2;
  let left = Math.round(cx - needW / 2);
  let top = Math.round(cy - needH / 2);
  // `sharp`'s `extract` rejects a zero-area region, which `Math.min(width, …)`
  // can produce when the image itself is smaller than the scaled minimum —
  // clamp to at least one device pixel so a small screenshot still crops.
  const w = Math.max(1, Math.min(width, Math.round(needW)));
  const h = Math.max(1, Math.min(height, Math.round(needH)));
  left = Math.max(0, Math.min(left, width - w));
  top = Math.max(0, Math.min(top, height - h));
  return { left, top, width: w, height: h };
}

/** Composite the callouts onto `png` and cut the crop; both as PNG buffers. */
export async function annotate(png, callouts, { scale = 1 } = {}) {
  const image = sharp(png);
  const { width, height } = await image.metadata();
  const svg = Buffer.from(overlaySvg({ width, height, callouts, scale }));
  const full = await image.composite([{ input: svg, top: 0, left: 0 }]).png().toBuffer();
  const region = cropBox({ width, height, callouts, scale });
  const crop = await sharp(full).extract(region).png().toBuffer();
  return { full, crop, region };
}

function escapeXml(s) {
  return s.replace(/[<>&"']/g, (c) => ({ "<": "&lt;", ">": "&gt;", "&": "&amp;", '"': "&quot;", "'": "&#39;" })[c]);
}
