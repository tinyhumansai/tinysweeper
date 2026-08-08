## Dependency and manifest changes

### Report

- A new dependency that duplicates something the project already has, or that
  the standard library covers. Name the existing one.
- A dependency added to the always-compiled set when the feature it serves is
  optional. In this project that breaks the offline default build, which is a
  load-bearing invariant and not a preference.
- A version requirement loosened in a way that admits a major version: `"1"` to
  `"*"`, an exact pin replaced by a range on a crate that has broken things
  before.
- A dependency pulled from a git revision or a path outside the repository,
  where a registry release exists.
- A lockfile change that alters versions the manifest did not ask to change,
  where the diff has no accompanying reason.
- A new transitive HTTP client, TLS stack or async runtime arriving through an
  otherwise small dependency. Say which dependency brings it.

### Do NOT report

- A routine version bump within the declared range, or a lockfile refresh whose
  manifest change explains it.
- The mere fact that a dependency is new. Adding one is normal; adding a
  redundant or badly scoped one is the finding.
- Crate popularity, download counts, maintenance status, or "consider whether
  you need this dependency" as a general observation.
- Feature flags added to an existing dependency, unless they pull in a runtime
  the project deliberately keeps out.
- Dev-dependencies and build-dependencies, unless they end up in the shipped
  artifact.
- Anything in a vendored directory or a submodule, which is upstream's decision
  and not this pull request's.
