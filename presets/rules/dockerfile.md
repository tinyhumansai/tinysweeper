## Dockerfile

### Report

- A secret — a credential, API key, or private key — copied into a layer or
  passed as a build `ARG`/`ENV`. It stays in the image history even if a
  later layer deletes the file.
- `ADD` pulling from a remote URL, which fetches and extracts unauthenticated,
  unpinned content at build time. Use `curl`/`wget` with a checksum, or a
  local `COPY`, instead.
- A base image tag that floats — `latest`, or no tag at all — where the
  project pins other dependencies. The build is no longer reproducible.
- A multi-stage build that copies a build-time secret or credential from an
  earlier stage into the final image instead of only the built artifact.
- Package installation with no version pin (`apt-get install foo` with no
  `=version`) directly above a `COPY`/`ADD` of application code, where the
  project pins elsewhere.

### Do NOT report

- Running as root. Flag it only when the image also drops privileges
  elsewhere inconsistently, or the project's own convention (documented or
  visible in sibling Dockerfiles) is non-root and this one deviates.
- A missing `HEALTHCHECK`. That is an orchestrator's job in most deployments
  and not a defect in the image itself.
- Layer count, `RUN` command chaining, or cache-friendliness of instruction
  order — performance preferences, not correctness.
- A pinned base image using a digest instead of a tag, or a tag instead of a
  digest — both are pins; neither is wrong.
- `EXPOSE` documenting a port the container does not enforce — it is
  documentation, not a security control.
- Build arguments that are not secrets (version numbers, feature flags) left
  without a default.
