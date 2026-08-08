## Tests

You are reading tests, not running them. Judge whether each one could fail.

### Report

- An assertion that cannot fail: `assert!(true)`, comparing a value to itself,
  asserting a literal you just wrote, or asserting that a constructor returned
  something.
- A test with no assertion at all, where the only failure mode is a panic the
  code under test does not raise.
- A test that asserts on a mock's recorded calls when the behaviour under test
  is what the real code computes. Verifying that you called the function you
  just called proves nothing about the answer.
- A new branch in the changed source — an early return, an error arm, a new
  boundary — where no changed test drives it. Name the branch and the line.
- A test whose name claims one thing and whose body checks another. The name is
  what a future reader trusts when it fails.
- An error path added to the source with no test that produces the error.
- A test made to pass by weakening it: a tightened assertion loosened, an
  expected value edited to match new output without the behaviour change that
  justifies it, a case commented out or marked ignored.

### Do NOT report

- Missing tests for a change with no behavioural component: documentation,
  comments, formatting, renames, import reordering, dependency version bumps
  with no code change.
- Missing tests for generated code, for a `Display`/`Debug` impl, for a plain
  data struct, or for a getter that returns a field.
- Test style: naming conventions, `assert_eq!` versus `assert!`, table-driven
  versus one function per case, how the fixtures are built, how long the file
  is.
- A test that is thorough about something you would not have prioritised. Extra
  coverage is not a defect.
- Duplication between tests. Tests are allowed to repeat themselves; a shared
  helper that hides what a test asserts is the worse outcome.
- The absence of an integration or end-to-end test, unless the repository's own
  policy asks for one.
- A branch that cannot regress silently — an `unreachable!`, an exhaustiveness
  arm the compiler enforces, a match on an enum the compiler already checks.
