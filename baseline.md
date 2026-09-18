# attempt_0001 — baseline (measured state BEFORE this attempt)

Measured 2026-09-17, at session start.

## Repository state

- No prior repository, no prior code, **no prior history directory anywhere on the
  filesystem** (verified with a filesystem-wide `find` for `*history*`, `attempt_0*`,
  `mpodol*` — zero hits outside the two uploaded source documents).
- Therefore: this is `attempt_0001`. There is no failure record to read back yet.

## Tests

- Test count: **0**. There is no test suite, so there is no pass/fail baseline to
  regress against. Any number > 0 passing at the end of this attempt is an improvement;
  the meaningful baseline starts at attempt_0002.

## Performance numbers

- **None, and none obtainable in this session.** Recording this explicitly because the
  plan's Phase 0 is defined by three measurements (tokens/sec, RAM, invalid-action rate)
  and I want the record to show clearly that they are *missing*, not *bad*:
  - No Raspberry Pi or any ARM hardware is reachable from this session.
  - No GGUF model weights are obtainable: the sandbox egress allowlist covers
    crates.io / PyPI / npm / GitHub / Ubuntu archive only. `huggingface.co` is **not**
    allowlisted, so no weights can be downloaded.
  - No `llama.cpp` build exists here (no `cmake`; buildable from GitHub in principle,
    but pointless without weights).

## Toolchain state (and what I had to do to it)

This is worth recording because a future session will land in a **fresh container with
none of it**, and re-deriving it costs several turns:

| Tool | Found at start | Notes |
|---|---|---|
| `rustc` / `cargo` | **MISSING** | The plan (§5) mandates Rust for the core. |
| `rustup` | MISSING | Its CDN (`static.rust-lang.org`) is **not** in the egress allowlist, so the normal install path is closed. |
| root / `sudo` | MISSING | Cannot `apt-get install`. |
| `gcc` / `cc` | 13.3.0 | Present. |
| `python3` | 3.12.3 | Present. |
| `pdftotext` | present | Used to derive `mpodol-plan.md` from the PDF. |
| `sqlite3`, `cmake` | MISSING | `sqlite3` will matter for sqlite-vec (§3.5) later. |

**How Rust was obtained** (reproducible, see `scripts/bootstrap-toolchain.sh`):
`apt-get install --print-uris -y rustc cargo` to resolve the URI set, `curl` the 8
`.deb`s from the allowlisted `archive.ubuntu.com`, then `dpkg -x` each into
`/home/claude/.local/rust` and put `usr/bin` on `PATH` with `usr/lib` on
`LD_LIBRARY_PATH`. Result: **rustc 1.75.0, cargo 1.75.0** — no root required.

Consequence to carry forward: **rustc is 1.75 (Dec 2023), not current.** Crates
requiring edition 2024 or a newer MSRV will not build. Dependency versions must be
pinned to 1.75-compatible releases (verified working: `serde 1.0.210`,
`serde_json 1.0.128`). Fetching from crates.io works.

## Environment shape

1 CPU core, 4 GB RAM. Coincidentally close to the plan's **Micro tier** (Pi 4, 4 GB) —
useful for sanity but it is x86_64, so it proves nothing about ARM.
