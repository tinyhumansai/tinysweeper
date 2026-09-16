## Shell

### Report

- An unquoted expansion — `$var`, `$@`, a command substitution — used where
  the value can contain whitespace, a glob character, or come from user input
  or a filename. Word-splitting and globbing turn one argument into several.
- `rm -rf "$path"` (or any destructive command) where `$path` can be empty or
  unset and the script has not checked, so it resolves to `rm -rf /` or the
  current directory.
- A script that would misbehave on a command failure or an unset variable it
  does not itself check for, and lacks `set -euo pipefail` (or the
  equivalent) to stop it. Judge this by what the script actually does, not by
  the flag's absence alone.
- A command substitution or heredoc built from a source outside the script
  (an argument, an environment variable, a file the script does not control)
  fed straight into `eval` or a second shell invocation.
- A loop reading command output with a bare `for x in $(cmd)` where output
  can contain spaces or newlines the loop should treat as one item.
- A trap or cleanup step that runs only on the success path, leaking a temp
  file or a background process when the script exits early.

### Do NOT report

- Missing `set -euo pipefail` on a script that already checks every command
  it cares about, or whose failure is meant to be non-fatal (a best-effort
  cleanup step).
- An unquoted expansion on a variable the script has just assigned from a
  literal, an integer, or a value with no possible whitespace.
- `rm -rf` on a path built entirely from literals in the same line, with no
  variable in it.
- Style: two-space vs four-space indent, `[ ]` vs `[[ ]]` where both are
  valid in the script's declared shell, or quoting style beyond correctness.
- A `shellcheck` finding already visible as a suppressed, commented directive
  (`# shellcheck disable=...`) with a reason.
- Portability to a shell the script's shebang does not target (a bashism in a
  script that starts `#!/bin/bash`).
