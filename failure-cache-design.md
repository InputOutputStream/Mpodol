# Failure cache — design (directive §3 / §5)

Status: **designed, not implemented.** Implementation is attempt_0002. Written
now because §3 requires the mechanism to be *measured* on vs. off, and the shape
of the measurement constrains the shape of the thing being measured.

The mechanism: persist every `Error` event, and before the agent selects an
action, retrieve semantically similar past failures and inject a short concrete
reminder. The plan (§3.5) already specifies the storage — sqlite-vec, local,
offline. What follows is the part that isn't specified: what gets stored, how
retrieval is keyed, what gets injected, and how we find out whether it helps.

## 1. The cache is an index over the event log, never a second source of truth

`ErrorRecord` lives in the event stream (`event.rs`), and the cache is a
derived, rebuildable index over it. Two reasons this matters more than it
sounds:

- The event log is already the replay/crash-recovery source of truth (plan
  §3.1). A separately-writable failure store would drift from it, and after an
  OOM kill you would have two disagreeing histories with no way to arbitrate.
- A corrupted or stale index is then always recoverable: drop the table, replay
  the log, reindex. No migration, no data loss.

So: `Error` events are appended first and indexed second. `error_records()` in
`event.rs` is the reindexing entry point.

## 2. What gets embedded — the decision that makes or breaks retrieval

The obvious move is to embed the error text. **That is the wrong thing to
embed.** Embedding `reason` clusters failures by how similarly they are
*worded*, and produces retrievals like "these two errors both mention a path."
What the agent needs is the opposite: failures that arose in a similar
*situation*, whatever their wording.

So the embedded string is a composed **situation key**, not the error prose:

```
step_intent | tool | salient args
```

e.g. `Restart the telemetry agent service. | shell_exec | systemctl restart telemetry-agent`

Salient args are the ones carrying situational meaning — `path`, `cmd`, `url`,
`query` — and explicitly *not* `content` (a 4 KB file body would swamp the
embedding) and not `timeout_ms`. The `code` and `repair_hint` are stored as
retrievable columns but are **not** part of the embedded text.

Three retrieval tiers, cheapest first:

| Tier | Key | Cost | Answers |
|---|---|---|---|
| 0 | `args_fingerprint` exact match | O(1) index hit | "I have tried this exact call before" |
| 1 | `(step_id, tool, code)` | O(1) index hit | "this kind of call fails at this step" |
| 2 | vector similarity over the situation key | one embedding + ANN | "something like this failed somewhere like here" |

Tier 0 is the one most likely to earn its keep, and it needs no embedding model
at all. That matters for device tiers (below).

## 3. Device tiering: tier 2 is not affordable on a Pi

Running an embedding model beside a 1B SLM on a Pi 4 with 4 GB competes for the
RAM budget the plan is explicit about. So retrieval degrades by device tier
(plan §4):

- **Micro** — tiers 0 and 1 only, plus a lexical fallback: Jaccard overlap over
  path segments and argv0 tokens. No embedding model loaded, no extra RAM.
- **Budget / Standard** — all three tiers, embeddings from a small dedicated
  model (the plan already contemplates a "tiny dedicated summarizer model" in
  §3.5; same slot).

This is a real prediction to test, not a hedge: **most of the value is probably
in tier 0.** An agent looping is usually re-emitting the *same* call, not a
semantically adjacent one. If the on/off measurement shows tier 0 capturing most
of the benefit, tier 2 should be cut rather than kept for elegance.

## 4. Injection

Retrieval runs **before** action selection, keyed on the current step: query =
that step's `intent` plus its permitted tool names.

Rules, all of them driven by the fact that the context window is tiny:

- **At most 2 reminders**, hard cap. On a 2 K-context model, a third reminder
  costs more task context than it can plausibly save.
- **The injected text is the stored `repair_hint` verbatim**, prefixed with a
  short frame. `repair_hint` is already written for the model, already concrete,
  and already names the mechanism. Re-generating a warning from the `reason`
  would mean an LLM call to restate something we already have in better form.
- **Injected into the scratchpad, not the transcript.** Plan §3.5 keeps the
  scratchpad separate and injects it fresh each turn; a reminder in the
  transcript would be replayed and re-summarized forever, and would eventually
  read as something the *user* said.
- Format:
  ```
  Avoid repeating: last time at this step, {tool} with {salient arg} failed — {repair_hint}
  ```

### Supersession — the rule that keeps the cache from poisoning fixed paths

A retrieved failure is **suppressed if a later successful `ActionExecuted` event
has the same `args_fingerprint`.** Without this, the first thing the cache does
is warn the agent away from the action that now works — actively worse than
having no cache. "Read your own garbage back" has to mean the garbage that is
still garbage.

## 5. Eviction

Per-step row cap (default 64), evicting oldest-first, plus a global cap keyed on
disk budget. A Pi with a 16 GB SD card cannot host an unbounded failure history,
and an unbounded one would also slowly degrade retrieval precision.

## 6. How we find out whether any of this helps

§3 requires a config flag and an on/off measurement, and is right that this may
backfire. The realistic failure modes, each with the metric that would catch it:

| Risk | What it looks like | Metric |
|---|---|---|
| **Over-caution** | model avoids a *valid* action because something superficially similar failed | valid-action rate, and task completion rate — must not drop |
| **Context starvation** | reminders displace task context on a 2 K window | prompt tokens spent on reminders; turns-to-completion |
| **False retrieval** | tier 2 surfaces unrelated failures | precision@2 against a hand-labeled retrieval set |
| **Confabulated pattern** | model over-generalizes one failure into a rule ("paths never work") | count of steps abandoned without any action attempted |

Primary metric: **repeated-error rate** = fraction of `Error` events whose
`(step_id, args_fingerprint)` pair matched an earlier `Error` event. Reported
with the cache on and off, over the same runbook set with a fixed seed.

The guardrail metrics are not optional garnish. A cache that drives the repeated-
error rate to zero by making the agent too timid to act has made things worse,
and would look like a total success on the primary metric alone.

Config: `failure_cache = off | tier0 | tier1 | full`, so the tiers can be
measured separately rather than as one lump — otherwise a win from tier 0 and a
loss from tier 2 net out to "no effect" and we learn nothing.

## 7. Deliberately unresolved

- Cross-run vs. within-run scope. Within-run is clearly useful. Cross-run risks
  carrying stale environment facts ("port 8080 was busy") into a run where they
  are false. Leaning toward cross-run retrieval with a recency decay, but this
  needs the measurement rig before it can be settled rather than guessed.
- Whether `Observation` events indicating *semantic* failure (command ran, exit
  code 0, wrong outcome) should be cached too. They are the most valuable
  failures and the hardest to detect. Out of scope until the runtime exists.
