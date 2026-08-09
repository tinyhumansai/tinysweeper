#!/usr/bin/env python3
"""Run the image's own entrypoint with the kernel gate turned off.

The community-server image refuses to start `mongod` on Linux 6.19 and newer.
The cause is a tcmalloc/glibc interaction around restartable sequences, not
anything MongoDB does differently on those kernels, and the image's own
docstring for the check says it stops startup "unless an explicit
degraded-performance bypass is set" — but no such bypass exists in
8.2.12-ubi9. `enforce_kernel_compatibility` prints and calls `sys.exit(1)`
with nothing to consult.

That leaves a developer on a current kernel unable to run `docker compose up`
at all, which is how this repository's own compose file came to be written but
never executed end to end.

This shim executes the real entrypoint with that one function replaced by a
no-op. Everything else — replica set initiation, `/docker-entrypoint-initdb.d`,
argument handling — is the image's, unmodified. The overlay that mounts it also
sets `GLIBC_TUNABLES=glibc.pthread.rseq=0`, which is the actual mitigation for
the underlying issue rather than merely silencing the check.

Opt-in, in a separate compose overlay, and never the default: on a kernel below
the cutoff this file should not be used, and on a supported production host it
must not be.
"""

import runpy
import sys

ENTRYPOINT = "/usr/local/bin/docker-entrypoint.py"


def main() -> None:
    source = open(ENTRYPOINT, encoding="utf-8").read()

    # Replace the body rather than deleting the call: the call site sits in the
    # image's `__main__` block between the architecture warning and replica-set
    # parsing, and editing that block would be a much larger surface to keep in
    # step with a future image.
    needle = "def enforce_kernel_compatibility() -> None:"
    if needle not in source:
        # The image changed shape. Fail loudly rather than starting a database
        # with an assumption that no longer holds.
        sys.exit(
            "kernel-bypass: this image no longer defines "
            "enforce_kernel_compatibility; re-check whether the bypass is still "
            "needed before editing this shim"
        )

    patched = source.replace(
        needle,
        "def enforce_kernel_compatibility() -> None:\n"
        "    return  # patched out by deploy/mongo/kernel-bypass-entrypoint.py\n"
        "\n"
        "def _enforce_kernel_compatibility_unused() -> None:",
        1,
    )

    globals_dict = runpy.run_path.__globals__  # noqa: F841 - keep runpy imported
    compiled = compile(patched, ENTRYPOINT, "exec")
    exec(compiled, {"__name__": "__main__", "__file__": ENTRYPOINT})  # noqa: S102


if __name__ == "__main__":
    main()
