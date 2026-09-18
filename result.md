# attempt_0001 — result

**Verdict: succeeded, scoped as proposed.** The pass/fail test defined in
`proposal.md` passes: `./scripts/test.sh` exits 0 with **43 tests passing, 0
failing, 0 ignored**, and 0 compiler warnings. Every test named in the proposal
exists and passes. Phase 0a (the deterministic instrument) is complete.

Logging this even though it succeeded, per directive §1 — future-me needs to know
what is already solid so it does not get "fixed" again.

## What was built

2,335 lines across five modules, tests inline:

| Module | Lines | Contents |
|---|---|---|
| `src/validate.rs` | 778 | two-pass validator, 16 reject codes, lexical path normalization |
| `src/grammar.rs` | 479 | step-scoped GBNF compiler |
| `src/event.rs` | 456 | append-only JSONL log, crash-tolerant replay, `ErrorRecord` |
| `src/action.rs` | 326 | 6-tool vocabulary, strict parser, FNV-1a fingerprints |
| `src/runbook.rs` | 250 | step contracts, `lint()`, worked example |

Plus `scripts/check-no-native-arch.sh` (self-testing), `scripts/test.sh` (the
phase gate), `scripts/bootstrap-toolchain.sh` (reproduces the no-root Rust
install), `docs/failure-cache-design.md`, `docs/open-issues.md`.

## What went wrong, in order

**1. `const fn` in array literals doesn't get lifetime-promoted.** `Tool::args()`
returned `&'static [ArgSpec]` built from `&[a("path", ArgKind::Path)]` — six
E0515s, because a `const fn` call inside an array literal in a normal function
body creates a temporary rather than a promoted static. Fixed by hoisting each
tool's arg list into a named `const` item. Ordinary compile error, ten minutes,
no design consequence. Noted only because a future session refactoring `args()`
will hit it again if it inlines those arrays back.

**2. One test failed, and the test was wrong, not the code.**
`grammar_bakes_argv0_allowlist` asserted `!g.gbnf.contains("rm")` — intending
"no command other than `systemctl` leaks into the grammar". It failed on the word
"Pe**rm**it" in a comment line of the generated grammar's own header.

The lesson is not "fix the assertion". It is that **asserting the absence of a
short substring across a whole generated artifact is not a test of anything** —
it passed for the wrong reason until a comment changed, and would have kept
passing while a real extra alternative was added. Replaced with an exact
equality check on the `a0-cmd` rule line, which can actually see an extra
alternative. Worth generalizing: every remaining `contains` assertion in
`grammar.rs` is checking for *presence* of a long, specific literal, which is a
defensible use; absence checks now compare whole rule lines.

**3. Inspecting the generated grammar found a real gap the tests had not.**
Dumping output (`cargo run --example dump_grammar`) showed the argv0 rule as:

```
a0-cmd ::= "\"systemctl" char* "\""
```

which also admits `systemctlfoo` — the allowlist was constraining a *prefix*, not
a command. Nothing in the test suite would have caught it; the semantic pass
would have (argv0 tokenizes to `systemctlfoo`, not in allowlist), so it was never
a hole, but it was needlessly samplable. Fixed by anchoring argv0 as two explicit
alternatives:

```
a0-cmd ::= "\"systemctl\"" | "\"systemctl " char* "\""
```

and pinned by a new test, `argv0_is_anchored_not_merely_prefixed`.

**Process note worth carrying forward: reading the generated artifact found a
defect that 42 passing tests did not.** For a component whose entire output is a
generated text file, dumping and reading that file is a first-class check, not a
debugging convenience. `examples/dump_grammar.rs` exists for that reason.

## Open issues left unresolved (nothing here is "done")

All in `docs/open-issues.md`; the two that most constrain what is safe to claim:

- **P0-2: the generated GBNF has never been parsed by llama.cpp.** Its
  correctness currently rests on my reading of the GBNF subset plus unit tests. A
  grammar that is well-formed to me and rejected by the real parser fails *on the
  device*. **This is the highest-value next task and is not blocked by the
  missing weights** — `gbnf-validator` needs a llama.cpp build, not a model.
- **P0-4/P0-5: path roots are a correctness guard, not yet a security boundary.**
  Lexical normalization cannot see a symlink, and there is a TOCTOU window
  between validation and execution. Both are the runtime's job
  (`openat2(RESOLVE_BENEATH)`, validate on the fd actually used), and the runtime
  does not exist. Stated in `validate.rs` at the function itself so it cannot be
  read as a solved problem.

Also unresolved and deliberately so: Phase 0's three measurements (P0-1), the
failure cache implementation (P0-7, designed only), and everything in Phases 1–3.

## What I would tell the next attempt

The proposal's claim that the two enforcement layers are each independently
load-bearing held up under implementation, and got sharper: three separate cases
now exist where the grammar admits something the semantic pass must reject
(path traversal past a baked-in root; `timeout_ms: 9999` under a 4-digit bound
with a 5000 ceiling; `systemctl restart x; rm -rf /` past an argv0 allowlist).
That is no longer an argument, it is three tests. Keep the layers independent.

Recommended next attempt, in order of value:

1. **P0-2** — build llama.cpp, run `gbnf-validator` over every grammar this crate
   can emit, add it to `scripts/test.sh`. Unblocked, cheap, and it retires the
   largest unproven assumption in the codebase.
2. **Failure cache, tier 0 only** (`docs/failure-cache-design.md` §2) — exact
   `args_fingerprint` match, no embedding model. The design predicts most of the
   value is here; build the cheap tier first and make the measurement rig prove
   or kill the expensive one rather than assuming.
3. **The repair loop** — its inputs are already shaped for it
   (`Reject.repair_hint`, `ParseError.repair_hint` are both written for the model
   and name the mechanism), and it is the last piece needed before a mock backend
   can produce an end-to-end invalid-action-rate number.

Do **not** start the sandbox runtime before the repair loop: it is the largest
piece, it is the one that must be rootless from line one, and it does not need to
exist to get the first reliability measurements out of the harness.
