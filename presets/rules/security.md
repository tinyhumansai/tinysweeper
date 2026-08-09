# Security rules

For the `security` lane. Half of this document is the list of things **not** to
report; that half is where the precision comes from.

## Report only a reachable path, not a shape

A finding needs three things, and all three must be visible in the diff or in
the retrieved context:

1. **A source** — data an attacker controls: a request parameter, header,
   cookie, body, filename, environment of a multi-tenant service, a webhook
   payload, a message off a queue, a field of a record another tenant can write.
2. **A sink** — where that data does damage: a process spawn, a shell string,
   a query, a template, a deserializer, a file path, an outbound URL, markup, a
   redirect target, a permission decision.
3. **A path from one to the other** with no adequate check in between.

If you cannot name all three, you do not have a finding. "This function uses
`exec`" is a shape. "`req.query.name` reaches `exec` unquoted on line 12" is a
finding.

## Classes worth the attention

Ordered by how often the class is both real and missed in review:

- **Injection into an interpreter** — shell, SQL, NoSQL operators, LDAP, XPath,
  template engines. Concatenation or interpolation of a source into a query or
  command; a parameterised query with one interpolated identifier is still this.
- **Authorisation moved or missing** — an object reference taken from the
  request and used without binding it to the caller; an added expansion or
  projection knob (`include`, `expand`, `fields`) that walks past the check; a
  new endpoint next to guarded siblings that has no guard.
- **Deserialization and dynamic evaluation** — untrusted bytes into a native
  deserializer, `eval`, a dynamic import, or a YAML loader that constructs
  arbitrary types.
- **Path handling** — a source used to build a filesystem path, an archive entry
  written without checking where it lands, a redirect or fetch target taken from
  a parameter (SSRF and open redirect are the same mistake at different layers).
- **Secrets and credentials** — a credential moved into a place it will be
  logged, echoed into an error, or committed. **Report the type and location
  only. Never quote the value, not even part of it, not even one already visible
  in the diff.**
- **Crypto and tokens** — a signature check made conditional, an algorithm or
  issuer/audience check dropped, a comparison of secrets that is not constant
  time, a nonce or IV that repeats.
- **Trust boundaries in the repository's own automation** — a workflow trigger
  that runs contributor code with secrets, a widened permission, an unpinned
  third-party action, an agent instruction file that tells a tool to do
  something it should not.

## Do NOT report

Each of these has produced a real false positive somewhere. They are the rule,
not the caveat.

- **A containment fixture.** A hostile string, injection payload or malformed
  input that a test feeds in *to assert that it is rejected, escaped or kept
  out* is the repository defending itself. Do not report it and never advise
  deleting it — deleting it deletes the proof the defence works. The exception
  is narrow: it needs a surrounding assertion about that very string. A live
  credential committed in a test that asserts nothing about it is still a
  finding.
- **Code the diff only moved.** A line that shifted because something above it
  grew is not a change. If the same code existed before this pull request, it is
  not this author's problem, however wrong it looks.
- **A sink with a literal argument.** `Command::new("ls")` with no interpolation
  has no source. The same goes for a query built entirely from constants.
- **A source that is already bound or validated.** An identifier taken from the
  session, from a verified token claim, or checked against an allowlist you can
  see, is not attacker-controlled by the time it reaches the sink.
- **Defence in depth you would merely prefer.** A missing second check where the
  first one holds is a suggestion, not a vulnerability. Do not raise it as high
  severity.
- **Framework behaviour you are guessing at.** An ORM's parameter binding, a
  template engine's auto-escaping, or a framework's CSRF middleware being absent
  from *this file* is not evidence it is absent from the application.
- **Test-only weakness that stays in tests.** A short key, a disabled TLS check
  or a fixed seed inside a test harness is a deliberate choice; report it only
  when the weakened setting is reachable from production code.
- **Dependency versions.** A lockfile bump is adjudicated deterministically
  before you see it. Do not speculate that a version "may contain" a
  vulnerability.
- **Denial of service by resource use.** An unbounded loop or allocation in a
  batch job is a performance finding. Raise it as security only when an
  attacker controls the size and the process serves other tenants.
- **Anything you would need to run the code to confirm.** We never execute the
  contributor's code. If confirming would take a request, a payload or a
  debugger, say what you saw and give it low confidence — do not assert it.
- **A missing security header or hardening option** on a file that does not
  configure them.

## Severity

- **Critical / high** — a reachable path from an attacker-controlled source to
  a damaging sink, or a credential that leaked.
- **Medium** — a real weakening of an existing control whose exploitation needs
  a precondition you cannot see.
- **Low** — hardening. Most "consider also validating…" findings are low, and
  most low findings are better left unsaid.

Confidence is not a hedge. Use it to say how much of the path you could
actually see.
