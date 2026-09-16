## Python

### Report

- A mutable default argument — `def f(x=[])`, `def f(x={})` — where the
  function mutates it. The default is built once and shared across every call
  that does not pass its own.
- A bare `except:` or a broad `except Exception:` that swallows the error
  silently (`pass`, or a log line with no re-raise) instead of handling or
  propagating it.
- `subprocess` called with `shell equals true` on a command built from a source that
  is not a fixed literal.
- `eval`, `exec`, `pickle.loads`, or `yaml.load` without `SafeLoader` on data
  that did not originate inside this process.
- A blocking call — synchronous I/O, `time.sleep`, `requests`, CPU-bound work
  — inside an `async def`, stalling the event loop for every other task.
- An `asyncio` task created with `asyncio.create_task` or `ensure_future` and
  never awaited or stored, so its exception is dropped and it can be
  garbage-collected mid-flight.
- SQL assembled with an f-string or `+` concatenation instead of a
  parameterised query, where any operand is not a literal.

### Do NOT report

- `except Exception:` at a process or task boundary — a request handler, a
  worker loop, a CLI entry point — that logs and continues, which is the
  correct place to stop an unknown failure from taking the whole process down.
- A mutable default argument on a function the diff shows is never called with
  the default, or that never mutates it.
- `shell equals true` on a command built entirely from literals in the call itself.
- Comparing to `None` with `==` instead of `is`. Correct in CPython, and not
  worth a comment.
- Missing type hints, `Optional` vs `| None`, or other style already covered
  by the project's formatter and linter.
- A blocking call inside a function that is never awaited concurrently with
  anything else — a script's `main`, a one-shot migration, a test fixture.
- `assert` used inside a test file. That is exactly what test assertions are.
- Performance concerns — a `list` where a `set` would be faster, a comprehension
  vs. a loop — with no evidence of hot-path or input-scaled cost.
