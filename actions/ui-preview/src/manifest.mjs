// The manifest the server validates, and the job summary people read.
//
// The manifest is the run's table of contents: which flows ran, how they
// ended, which files hold their pictures. Paths are relative to the run
// prefix and the server composes the URLs — the manifest never carries one.
// The job summary is the same gallery written into the workflow run, so the
// pictures are visible even when the server is unreachable.

/** Build the manifest from what the flows produced. */
export function buildManifest({ repo, pullRequest, headSha, baseSha, run, flows }) {
  return {
    version: 1,
    repo,
    pull_request: pullRequest,
    head_sha: headSha,
    base_sha: baseSha,
    run,
    flows: flows.map((flow) => ({
      id: flow.id,
      title: flow.title,
      status: flow.status,
      ...(flow.failedAt !== undefined ? { failed_at: flow.failedAt } : {}),
      ...(flow.clip ? { clip: flow.clip } : {}),
      changes: flow.changes.map((change) => ({
        n: change.n,
        step: change.step,
        path: change.path,
        full: change.full,
        crop: change.crop,
        ...(change.before ? { before: change.before } : {}),
        callouts: change.callouts.map((c) => ({ n: c.n, label: c.label })),
      })),
    })),
  };
}

/** The GitHub job summary, as Markdown with the same two-column table. */
export function jobSummary(manifest, baseUrl) {
  const prefix = baseUrl ? `${baseUrl.replace(/\/+$/, "")}/${manifest.repo}/${manifest.head_sha}/${manifest.run}/` : null;
  const cells = [];
  for (const flow of manifest.flows) {
    if (flow.status === "failed") continue;
    const tag = flow.status === "before_failed" ? " <sub>new in this PR</sub>" : "";
    if (flow.clip && prefix) {
      cells.push(
        `<a href="${prefix}${flow.clip.video}"><img src="${prefix}${flow.clip.gif}" width="380" alt="${esc(flow.title)}"></a><br><b>${esc(flow.title)}</b>${tag}`,
      );
    }
    for (const change of flow.changes) {
      cells.push(
        prefix
          ? `<a href="${prefix}${change.full}"><img src="${prefix}${change.crop}" width="380" alt="${esc(change.path)}"></a><br><b>${esc(flow.title)}</b>${tag}`
          : `<b>${esc(flow.title)}</b>${tag}<br><code>${esc(change.crop)}</code>`,
      );
    }
  }
  let out = `### 🎬 UI preview — PR #${manifest.pull_request}\n\n`;
  if (cells.length === 0) {
    out += "_No user flow produced a picture on this commit._\n";
    return out;
  }
  out += "<table>\n";
  for (let i = 0; i < cells.length; i += 2) {
    out += "  <tr>";
    for (const cell of cells.slice(i, i + 2)) {
      out += `<td width="50%" valign="top">${cell}</td>`;
    }
    out += "</tr>\n";
  }
  out += "</table>\n";
  return out;
}

function esc(s) {
  return String(s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
}
