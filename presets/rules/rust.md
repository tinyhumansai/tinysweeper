## Rust

### Report

- A `unwrap()`, `expect()` or `panic!` on a path reachable from library code.
  Library functions return the crate's `Result<T>`; a panic there takes the
  caller's process down over an error the caller could have handled.
- An `unsafe` block whose safety argument is not written down next to it, or
  whose stated invariant the surrounding code does not actually establish.
- A blocking call — `std::fs`, `std::net`, `std::thread::sleep`, a synchronous
  `Mutex` held across an `.await` — inside an `async fn`. It stalls the whole
  executor thread, not just that task.
- Integer arithmetic on a value derived from input where an overflow changes the
  result: indexing, capacity, length, offset. Say which operand can be large.
- A `clone()` inside a loop over a large or unbounded collection, where a borrow
  would do and the collection's size comes from input.
- An error swallowed: `let _ = ...` on a `Result`, or a match arm that discards
  an error without recording it.
- A public item whose signature changed in a way that breaks an existing caller
  — a new required argument, a narrowed return type, a removed variant on a
  non-`#[non_exhaustive]` enum.

### Do NOT report

- `unwrap()` or `expect()` in a `#[cfg(test)]` block, a test function, a
  benchmark, an example, or a `build.rs`. A panicking test is a failing test,
  which is the intended behaviour.
- `expect()` on something the program cannot continue without and that cannot
  vary at run time — a compiled-in constant that must parse, a mutex whose
  poisoning means the process is already broken. If the message says why, it is
  a documented invariant, not an oversight.
- Missing `#[derive(...)]`, missing `#[must_use]`, or any other attribute that
  is a preference rather than a defect.
- `clone()` on a `Copy`-sized value, an `Arc`, an `Rc`, or anything inside a
  path that runs once at start-up. Cloning is not a bug.
- Arithmetic on constants, on loop counters with a compile-time bound, or on
  values the code has just bounds-checked.
- Naming, module layout, formatting, import ordering, or where a `use` sits.
  `rustfmt` and `clippy` already ran and are not your job.
- `unsafe` in a module whose entire purpose is the unsafe abstraction, where the
  invariant is documented at the module level rather than at each block.
- A `.await` inside a loop. That is how you await things in a loop.
- Anything in generated code, a vendored dependency, or a file the diff only
  reformatted.
