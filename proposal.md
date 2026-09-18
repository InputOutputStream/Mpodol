# attempt_0001 — proposal (written before any code was written)

## What has already been tried

**Nothing. This is the first attempt on this project.**

`$history_dir` did not exist before this attempt; a filesystem-wide search found no
`attempt_*` directory, no prior repository, and no prior artifacts of any kind. The only
pre-existing files are the two uploaded source documents. So there is no failure record
to read back, no cluster of prior mechanisms to check for a local optimum (§2), and
nothing to avoid re-trying.

Because that makes this section vacuous exactly once, I am using it instead to record
the things a future attempt should **not** read as unexplored territory — i.e. decisions
taken here deliberately, so attempt_0002+ can tell "already settled" apart from
"never tried":

| Decision taken in this attempt | Status | Don't re-litigate unless |
|---|---|---|
| Rust core via `dpkg -x` of Ubuntu's rustc 1.75 | works, no root needed | a crate needs MSRV > 1.75 |
| Zero-dependency core except `serde`/`serde_json` | deliberate, for Pi footprint | a real need appears |
| GBNF emitted as generated text, not via a grammar-builder crate | deliberate | llama.cpp's own parser rejects it |
| Phase 0's three measurements | **blocked, not skipped** | weights + ARM hardware become reachable |

## Which phase, and an honest correction to the phase order

The plan's roadmap (§7) puts **Phase 0 — Spike** first: run llama.cpp + a 1–3B model on
an actual Raspberry Pi and measure tokens/sec, RAM, and how often small models emit
invalid actions. I cannot do that in this session and I am not going to pretend
otherwise — no ARM hardware is reachable and `huggingface.co` is outside the egress
allowlist, so no GGUF weights can be fetched (see `baseline.md`).

What I am doing instead is **not** skipping ahead to Phase 1 for convenience. Phase 0's
headline metric is "how often do small models fail to produce valid actions" — and that
number is undefined until something deterministic decides what *valid* means. The
validator and the grammar generator **are Phase 0's measuring instrument**. Building
them first is a prerequisite for Phase 0, not a substitute for it.

So: **Phase 0a — the deterministic, model-independent instrument.** Everything in this
attempt is a pure function over data structures, which is precisely why it can be tested
to completion with no model present.

Scope, mapped onto the innovation list in §5 of the directive:

1. **Event stream** (plan §3.1) — append-only JSONL log, `fsync` on append (Pi-class
   devices die from OOM and power loss; the log is the source of truth for replay), full
   replay. `Error` events carry a *structured* record, not a prose string, because §3's
   failure cache has to retrieve on those fields later.
2. **Per-runbook-step GBNF generation** (§5, innovation 1) — one grammar generated per
   step from that step's declared contract, never one global grammar.
3. **Semantic validator as a genuinely separate pass** (§5, innovation 2) — runs after
   parse, shares no code path with grammar generation, and is tested against actions the
   grammar *accepts*.
4. **A `-march=native` guard** (§5, innovation 3) — cheap now, and the failure mode is
   invisible (silently degraded binaries on every non-build machine), so a check that
   fails loudly belongs in the repo from line one rather than in Phase 3's CI.

## The design problem I actually have to get right

§5 warns that the constraint tax — syntactically valid, semantically wrong — is what
eats GBNF's reliability gains. That forces a decision about **where each constraint
lives**, and the split I am proposing is the substance of this attempt:

- **The grammar makes whole classes of wrong action unrepresentable.** Per step, it
  admits only that step's tools, in a fixed key order, and — the non-obvious part — it
  bakes each path argument's permitted root into the grammar *as a string literal*
  (`"\"" "/etc/mpodol/" [^"\\]* "\""`). The model cannot emit a path outside that root,
  because no such token sequence exists in the grammar. Same for shell `argv0`:
  the allowlist becomes literal alternatives.
- **The semantic pass adjudicates what a grammar structurally cannot.** A grammar is a
  regular/context-free shape over tokens; it cannot evaluate. It cannot know that
  `/etc/mpodol/../../etc/passwd` matches the baked-in prefix and still escapes the root
  after normalization. It cannot enforce a byte cap, a timeout ceiling, or step ordering.

These two facts are why both layers exist, and `/etc/mpodol/../../etc/passwd` is the
single test case that proves neither one alone is sufficient. **The semantic pass
re-checks the tool name too**, even though the grammar already narrowed it — defense in
depth, because the grammar is a reliability device and must not be load-bearing for
safety (a `--no-grammar` run, or any future non-llama.cpp backend, must remain safe).

## Concrete pass/fail test for this attempt

Falsifiable, so "done" is not my say-so (§4):

`cargo test` must be green, and must include tests that each fail if the corresponding
property breaks:

- `event_log_roundtrip` — events survive append → replay byte-identically, sequence
  numbers monotonic.
- `event_log_survives_truncated_tail` — a half-written final line (power loss) is
  dropped and the rest replays.
- `grammar_is_step_scoped` — two different steps of one runbook produce *different*
  grammars, each mentioning only its own step's tools.
- `grammar_bakes_path_root` — the permitted root appears as a literal in the grammar.
- `semantic_rejects_traversal_that_grammar_admits` — the `../..` escape case above:
  asserts the grammar admits the path shape *and* the semantic pass rejects it.
- `semantic_rejects_wrong_tool_independently` — a tool the grammar would never emit is
  still rejected by the semantic pass on its own.
- `semantic_accepts_intended_action` — the happy path, so the validator is not just
  trivially rejecting everything (a validator that rejects all input passes every
  negative test; this is the test that keeps it honest).
- plus arg-cap, timeout-ceiling, and step-ordering cases.

And `scripts/check-no-native-arch.sh` must exit non-zero when `-march=native` or
`target-cpu=native` is present anywhere in the build config.

## Explicitly deferred (with the reason, so it is not mistaken for an oversight)

- **The §3 failure cache.** I am designing it in this attempt (`docs/failure-cache-design.md`)
  and implementing it in attempt_0002. Reason: §3 requires its repeated-error rate to be
  *measured* on vs. off, and that measurement needs a backend that can be made to repeat
  an error deterministically. Wiring a vector store I cannot yet measure is exactly the
  "assume it helps" failure §3 warns against.
- **Rootless sandbox runtime** (plan §3.4). Not started. §5 is explicit that retrofitting
  rootless later touches permission assumptions everywhere, so when it starts it starts
  rootless — but it needs `unshare`/cgroup work that deserves its own attempt.
- **Model layer, router, device tiers, pip/Docker distribution.** Phases 1–3.
