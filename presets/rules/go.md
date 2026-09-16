## Go

### Report

- An error returned from a call and discarded: `_ = f()`, or a result used
  without checking the error that came with it. Name the call.
- A goroutine started with no way to observe its completion or stop it: no
  `WaitGroup`, no cancellation channel, no context — it outlives the request
  that spawned it.
- `defer` inside a loop that can run many iterations, delaying a `Close`,
  `Unlock` or transaction rollback until the function returns instead of each
  iteration.
- `context.Background()` or `context.TODO()` used on a request or worker path
  that has a caller's context to inherit, dropping its deadline and
  cancellation.
- A mutex, `sync.Once`, or other non-copyable synchronization primitive copied
  by value — through a value receiver, an assignment, or a struct passed by
  value — after it has been used.
- A `context.WithCancel`/`WithTimeout` whose `cancel` function is not called on
  every path once the derived context is no longer needed.
- Concurrent map or slice access with no lock and no channel serializing it,
  where the call sites show more than one goroutine can reach it.

### Do NOT report

- A missing error check on a call the standard library documents as
  infallible in context, such as `fmt.Fprintf` to a `strings.Builder`.
- `panic` or `log.Fatal` in `main`, in `init`, or in a CLI's top-level command
  handler, where there is no caller left to hand a `Result` to.
- A goroutine whose lifetime is deliberately the process's — a background
  ticker started once in `main` and never stopped.
- Missing `Stop()` on a `time.Timer` or `time.Ticker` alone, with no evidence
  it is created repeatedly or its owner is long-lived; the concern is a leak
  under repetition, not the call itself.
- `context.Context` stored in a struct field when the struct's own interface
  is fixed by something outside this diff (a generated client, an external
  contract) and passing it explicitly would break that contract.
- Formatting, import grouping, or anything `gofmt` and `go vet` already
  enforce.
- A `.await`-shaped blocking call inside a benchmark, a `_test.go` file, or a
  `main` used only to drive a local script.
