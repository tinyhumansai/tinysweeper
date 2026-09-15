// Uploading a run: to the server by default, to an object store when the
// operator configured one.
//
// The server path needs no credential beyond the session token — the server
// commits the files to a branch of the repository through the App at
// `finish`. The bucket path keys objects as
// `{owner}/{name}/{head_sha}/{run}/{file}`, exactly the prefix the server
// composes URLs from, so the two never have to agree on anything but the
// base URL; every object is content-addressed by commit and run, immutable,
// and cached as such.

import { readFile, readdir } from "node:fs/promises";
import path from "node:path";
import { PutObjectCommand, S3Client } from "@aws-sdk/client-s3";

const TYPES = {
  ".png": "image/png",
  ".gif": "image/gif",
  ".mp4": "video/mp4",
  ".webm": "video/webm",
};

/** Stage every asset in `dir` on the server. Returns the names sent. */
export async function uploadToServer({ dir, session, log = console.error }) {
  const files = (await readdir(dir)).filter((f) => path.extname(f) in TYPES && f !== "manifest.json");
  const names = [];
  for (const file of files) {
    await session.asset(file, await readFile(path.join(dir, file)), TYPES[path.extname(file)]);
    names.push(file);
    log(`[upload] ${file} → server`);
  }
  return names;
}

/** Upload every file in `dir` under `prefix`. Returns the keys written. */
export async function upload({ dir, prefix, bucket, endpoint, region = "auto", log = console.error }) {
  const client = new S3Client({
    region,
    endpoint,
    // R2 and most S3-compatible stores want the bucket in the path, not the host.
    forcePathStyle: true,
  });
  const files = (await readdir(dir)).filter((f) => path.extname(f) in TYPES);
  const keys = [];
  for (const file of files) {
    const key = `${prefix}/${file}`;
    await client.send(
      new PutObjectCommand({
        Bucket: bucket,
        Key: key,
        Body: await readFile(path.join(dir, file)),
        ContentType: TYPES[path.extname(file)],
        CacheControl: "public, max-age=31536000, immutable",
      }),
    );
    keys.push(key);
    log(`[upload] ${key}`);
  }
  return keys;
}
