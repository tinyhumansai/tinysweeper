// Uploading a run to the object store.
//
// Keys are `{owner}/{name}/{head_sha}/{run}/{file}` — exactly the prefix the
// server composes URLs from, so the two never have to agree on anything but
// the base URL. Every object is content-addressed by the commit and the run,
// so it is immutable and cached as such; a re-run writes a new prefix.

import { readFile, readdir } from "node:fs/promises";
import path from "node:path";
import { PutObjectCommand, S3Client } from "@aws-sdk/client-s3";

const TYPES = {
  ".png": "image/png",
  ".gif": "image/gif",
  ".mp4": "video/mp4",
  ".webm": "video/webm",
  ".json": "application/json",
};

/** Upload every file in `dir` under `prefix`. Returns the keys written. */
export async function upload({ dir, prefix, bucket, endpoint, region = "auto", log = console.error }) {
  const client = new S3Client({
    region,
    endpoint,
    // R2 and most S3-compatible stores want the bucket in the path, not the
    // host; AWS itself deprecates path-style and wants `bucket.s3.region…`,
    // which is also the shape of the public URL the server composes.
    forcePathStyle: !/\.amazonaws\.com$/.test(new URL(endpoint).hostname),
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
