## Java

### Report

- A checked exception caught and swallowed: an empty `catch` block, or one
  that logs and continues where the caller needed to know the operation
  failed.
- A `Closeable`/`AutoCloseable` resource — a stream, connection, lock — opened
  outside a try-with-resources or a `finally` that closes it, so an exception
  between open and close leaks it.
- `equals()` overridden without a matching `hashCode()`, or the reverse,
  breaking every hash-based collection the type is put into.
- A `catch (Exception e)` or `catch (Throwable t)` around a block that can
  throw several distinct failures, where the handler treats them all the
  same and the caller needed to distinguish them.
- A check-then-act sequence on shared mutable state — `if (map.get(k) ==
  null) map.put(k, v)` — with no synchronization, where the surrounding code
  shows more than one thread can reach it.
- A collection or field that is not thread-safe (`ArrayList`, `HashMap`)
  written from more than one thread with no external synchronization.
- A resource acquired in a `try` block whose `finally` closes a different
  variable, or closes it only on the success path.

### Do NOT report

- A caught exception logged and rethrown, or wrapped in a domain exception and
  rethrown — that is the exception being handled, not swallowed.
- Thread-safety concerns on a local variable, a builder mid-construction, or
  any object the diff shows never escapes a single thread.
- Read-only access to a shared, effectively-immutable field (`final`, set
  once in the constructor).
- Missing `equals`/`hashCode` on a class never put into a `Set`, used as a
  `Map` key, or compared with `.equals()` anywhere in the diff.
- Try-with-resources omitted on a resource the surrounding framework already
  owns and closes (a container-managed connection, a request-scoped bean).
- Checked-exception boilerplate, `throws` clause width, or choosing a checked
  over an unchecked exception — a design preference, not a defect.
- Formatting, brace style, or import order already enforced by the project's
  formatter.
