# Mpodol

An offline-first agent harness for small language models (1–8B, quantized),
targeting budget laptops and Raspberry Pi–class hardware. Architecture:
[`mpodol-plan.md`](mpodol-plan.md). Build discipline:
[`mpodol-implementation-prompt.md`](mpodol-implementation-prompt.md).

## If you are an agent starting or resuming work here, read this first

The directive's §6 bootstrap sequence, made concrete:

1. Read [`mpodol-plan.md`](mpodol-plan.md) in full.
2. Read **every** file under `.mpodol/history/` in full — every `attempt_*/proposal.md`,
   `baseline.md`, and `result.md`, not just the newest. This is the mechanism that
   keeps the build from looping; skipping it is how a project re-tries a dead idea
   for the fourth time.
3. Read [`docs/open-issues.md`](docs/open-issues.md).
4. Write your `## What has already been tried` section before proposing anything.
5. State which phase you are on and its concrete pass/fail test.
6. Only then write code.

`$history_dir` in the directive resolves to **`.mpodol/history/`** at the repo
root. Resolved here so every session picks the same directory and the record
stays in one place.

## State of the build

**Phase 0a complete** — the deterministic, model-independent instrument. 43 tests
passing. What exists:

| Module | What it is |
|---|---|
| `src/event.rs` | Append-only JSONL event stream (plan §3.1), `fsync` by default, crash-tolerant replay |
| `src/runbook.rs` | Runbook step contracts — what the operator authorized at each step |
| `src/grammar.rs` | Step-scoped GBNF compiler (directive §5.1) |
| `src/validate.rs` | Two-pass validator: structure, then step contract (directive §5.2) |
| `src/action.rs` | Fixed action vocabulary (plan §3.3), strict parser, stable fingerprints |

**Not built:** model layer, sandbox runtime, failure cache, repair loop, context
summarization, distribution. Phase 0's three headline measurements (tokens/sec,
RAM, invalid-action rate) are **unobtained** — see `docs/open-issues.md` P0-1 for
why, and do not mistake their absence for their being fine.

## The layering that matters

```
        Runbook step contract  (runbook.rs)
        what the operator authorized at this step
                 |                    |
          compiles to            enforced by
                 v                    v
      step-scoped GBNF          two-pass validator
        (grammar.rs)               (validate.rs)

   prevention:                 detection:
   makes wrong actions         catches what a grammar
   unsamplable at this step    cannot evaluate
```

One authorization source, two **independent** enforcement layers. They share the
contract and no enforcement code, deliberately:

- The grammar is a *reliability* device. It is switchable off, does not exist for
  non-llama.cpp backends, and only constrains sampling — an action replayed from
  a log never passes through it. So it must never be load-bearing for safety.
- The validator is the *safety* layer, and it re-checks even the things the
  grammar made unrepresentable, including the tool name.

The case that proves neither layer is redundant: at a step rooted in
`/etc/mpodol/`, the grammar bakes that prefix in as a literal, so no path outside
it is samplable — and `/etc/mpodol/../../etc/passwd` satisfies the grammar
perfectly while resolving to `/etc/passwd`. No context-free grammar can normalize
a path. Pinned by `semantic_rejects_traversal_that_grammar_admits`, which asserts
both halves.

## Building and testing

No Rust in the environment, no root, and rustup's CDN is not reachable — so:

```sh
./scripts/bootstrap-toolchain.sh          # unpacks Ubuntu's rustc/cargo into ~/.local/rust
export PATH=$HOME/.local/rust/usr/bin:$PATH
export LD_LIBRARY_PATH=$HOME/.local/rust/usr/lib/x86_64-linux-gnu:$HOME/.local/rust/usr/lib

./scripts/test.sh                          # the phase gate: guard + tests
cargo run --example dump_grammar           # see the per-step grammars
```

`scripts/test.sh` is the gate. A phase is done when it exits 0, not when an agent
says the phase is done.

Toolchain is **rustc 1.75** (Ubuntu 24.04 debs). Keep dependencies
1.75-compatible; see `docs/open-issues.md` P0-9.

## Things not to undo

- **No `-march=native` / `target-cpu=native`, ever.** Release binaries must run on
  hardware other than the build machine, and the failure mode is silent — SIGILL
  on a Pi, or working everywhere except some of the fleet.
  `scripts/check-no-native-arch.sh` enforces this and self-tests, so a green run
  means something. Use runtime CPU dispatch.
- **Rootless sandboxing from line one** when the runtime is written. Retrofitting
  it touches permission assumptions everywhere (`docs/open-issues.md` P0-6).
- **Path roots are not yet a security boundary** — lexical normalization cannot
  see symlinks. The runtime must enforce containment in the kernel
  (`openat2(RESOLVE_BENEATH)`). P0-4, P0-5.
- **`pkill`/`kill`/`killall` by name are off the table** (directive §0). Stop a
  specific PID you started, or ask.
