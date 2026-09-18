# Open issues

Known gaps, unproven assumptions, and things deliberately left undone. Per
directive §4, nothing here is a reason to call a phase complete and quietly move
on — this file is where "we know it's broken" lives so it does not get
rediscovered as a surprise.

Read together with `.mpodol/history/` before starting work.

## Blocking Phase 0 proper

**P0-1 — no model, no hardware, so none of Phase 0's three measurements exist.**
Phase 0 is defined by tokens/sec, RAM, and invalid-action rate on real hardware.
`huggingface.co` is outside this environment's egress allowlist, so no GGUF
weights can be fetched, and no ARM hardware is reachable. The instrument that
*produces* the invalid-action number is what attempt_0001 built; the number
itself is unobtained. Unblocking needs either an allowlist entry for a weights
host or weights supplied out of band, plus a Pi to run on.

## Unproven correctness

**P0-2 — generated GBNF has never been parsed by llama.cpp.** `grammar.rs`
targets a conservative GBNF subset (literals, `|`, `*`, negated classes; no
`{m,n}`, no `\x` escapes) and its output is verified only by this crate's unit
tests. A grammar that is well-formed by my reading and rejected by llama.cpp's
parser would fail *at runtime on the device*, which is the worst place to find
out. Fix: vendor llama.cpp's `gbnf-validator` example into CI and assert every
grammar this crate can emit parses. This is the single highest-value next check
and is cheap — it needs a llama.cpp build, not weights, so it is **not** blocked
by P0-1.

**P0-3 — `grammar_admits_path()` is a proxy, not a GBNF engine.** It encodes the
one structural property the generated path rule guarantees, so that
`semantic_rejects_traversal_that_grammar_admits` can assert both halves of its
claim. If the emitted path rule changes shape, the proxy must change with it, and
nothing currently enforces that. Superseded by P0-2.

## Known-insufficient security

**P0-4 — lexical path normalization cannot see symlinks.** `normalize_lexical()`
resolves `..` textually, which is necessary (the target may not exist yet, and
`canonicalize()` would hit the disk on every validation) but **not sufficient**:
a symlink at `/etc/mpodol/evil -> /etc/shadow` normalizes to a path inside the
permitted root and resolves outside it. Closing this belongs to the runtime,
which must open with `openat2(RESOLVE_BENEATH)` or `O_NOFOLLOW` per component so
the kernel enforces containment. **Until the runtime does that, path roots are a
correctness guard, not a security boundary.** Documented in `validate.rs` at the
function itself, not only here.

**P0-5 — TOCTOU between validation and execution.** Even with P0-4 fixed, a path
validated at time T can be replaced before it is opened at T+1. The runtime must
validate on the *file descriptor it actually uses*, not re-resolve the path.
Design constraint on the sandbox, recorded before the sandbox exists so it is not
retrofitted.

## Not started

**P0-6 — rootless sandbox.** Directive §5 is explicit that retrofitting rootless
later touches permission assumptions everywhere. Nothing is written yet, so
nothing assumes root yet; when the runtime starts it starts rootless
(user namespaces + cgroup v2 delegation, no privileged Docker path on the
Micro/Budget tiers). Recorded so a future session does not reach for the easy
root-assuming version first.

**P0-7 — failure cache.** Designed in `docs/failure-cache-design.md`, not built.

**P0-8 — model layer, router, device-tier detection, repair loop, context
summarization, pip/Docker distribution.** Phases 1–3. The repair loop has its
inputs ready (`Reject.repair_hint` is written for the model, `ParseError` too)
but no loop consumes them yet.

## Environment debt

**P0-9 — rustc 1.75 from Ubuntu debs, no root.** See
`.mpodol/history/attempt_0001/baseline.md` and
`scripts/bootstrap-toolchain.sh`. Dependencies must stay 1.75-compatible.
Cross-compilation to `aarch64-unknown-linux-musl` (plan §6) has **not** been
attempted and will likely need more than these debs provide.

**P0-10 — `panic = "abort"` in the release profile.** Correct for a small static
binary, but it means no unwinding, so any future in-process recovery from a
panicked action handler is impossible by construction. Fine while actions run
out-of-process in a sandbox; revisit if that ever stops being true.
