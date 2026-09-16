## TypeScript / JavaScript

Covers `.ts`, `.tsx`, `.js`, `.jsx`, `.mjs` and `.cjs`.

### Report

- A floating promise: an `async` call, or anything returning a `Promise`,
  invoked with neither `await` nor a `.catch`/`.then` handling its rejection.
  The failure disappears into an unhandled rejection.
- A missing `await` on a call inside an otherwise-awaited sequence, where the
  next line depends on the result and the type checker did not catch it
  because the return type is not `Promise<T>` at that boundary (a callback, a
  loosely-typed SDK).
- `any` at a module boundary — an exported function's parameter or return
  type, a public API's payload — that erases a caller's type safety past this
  file.
- User input assigned into an object via a computed key (`obj[key] = value`),
  or merged onto an existing object with `Object.assign`, without checking
  against `__proto__`, `constructor` or `prototype` — both go through
  `[[Set]]` and can reach the prototype-chain accessor. Not this when the
  target was created with `Object.create(null)` or is otherwise documented as
  a null-prototype dictionary: it has no `__proto__` accessor to reach, so
  those keys land as ordinary own properties. A fresh object literal's own
  spread (`{...a, ...b}`) is not this either: spread uses
  `CreateDataProperty`, which never invokes that setter, so `{...untrusted}`
  cannot repoint the result's prototype on its own — only report it where the
  spread result is later deep-merged into another object unsafely.
- `innerHTML`, `dangerouslySetInnerHTML`, or a template literal handed to
  `eval`/`Function`, built from a value that is not a fixed literal and has
  not passed through a sanitizer (`DOMPurify.sanitize` or an equivalent
  trusted-value guarantee) on the path shown in the diff.
- `Promise.all` used where the call site needs to observe every input's
  result or error individually — one rejection short-circuits the others
  silently, though it does not cancel their execution. Not a finding on its
  own for independent fire-and-forget work where only the aggregate success
  matters; use `Promise.allSettled` as the fix, not sequential `await`.
  Separately, a sequential `await` in a loop over independent async calls
  that should run concurrently is its own finding.
- A `.then()` chain or callback missing error handling, where a sibling
  `await` call in the same file does have a `try`/`catch`.

### Do NOT report

- `any` inside a function body or a test file, where it does not cross an
  exported boundary.
- A floating promise on a call whose only effect is fire-and-forget logging or
  telemetry, and whose rejection is deliberately ignored with a comment
  saying so.
- A shallow object-literal spread (`{...a, ...b}`) of untrusted data with no
  later unsafe deep-merge of the result — spread cannot itself repoint a
  prototype.
- Sequential `await` in a loop when each iteration depends on the previous
  result, or the collection is small and fixed at call time.
- `innerHTML`/`dangerouslySetInnerHTML` fed a value that has already passed
  through a sanitizer or is a fixed literal.
- `Promise.all` used purely to run independent work concurrently and wait for
  all of it, with no code path that needs a partial result after one
  rejection.
- `==` vs `===`, `var` vs `let`/`const`, missing semicolons, or anything the
  project's linter and formatter already enforce.
- Missing null checks on a value the type checker already narrows to
  non-nullable at that point.
- React hook dependency arrays, memoisation, or component structure, unless
  the diff shows a stale closure that produces a wrong value, not just a
  missed optimisation.
- `console.log` left in a test file, an example, or a script meant to be run
  locally.
