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
- User input assigned into an object via a computed key, or merged with
  `Object.assign`/spread, without checking against `__proto__`,
  `constructor` or `prototype` — prototype pollution.
- `innerHTML`, `dangerouslySetInnerHTML`, or a template literal handed to
  `eval`/`Function`, built from a value that is not a fixed literal.
- `Promise.all` used where one rejection should not cancel the independent
  work of the others (silent partial failure), or a sequential `await` in a
  loop over independent async calls that should run concurrently.
- A `.then()` chain or callback missing error handling, where a sibling
  `await` call in the same file does have a `try`/`catch`.

### Do NOT report

- `any` inside a function body or a test file, where it does not cross an
  exported boundary.
- A floating promise on a call whose only effect is fire-and-forget logging or
  telemetry, and whose rejection is deliberately ignored with a comment
  saying so.
- Sequential `await` in a loop when each iteration depends on the previous
  result, or the collection is small and fixed at call time.
- `==` vs `===`, `var` vs `let`/`const`, missing semicolons, or anything the
  project's linter and formatter already enforce.
- Missing null checks on a value the type checker already narrows to
  non-nullable at that point.
- React hook dependency arrays, memoisation, or component structure, unless
  the diff shows a stale closure that produces a wrong value, not just a
  missed optimisation.
- `console.log` left in a test file, an example, or a script meant to be run
  locally.
