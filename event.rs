//! The event stream (plan §3.1) — append-only, JSONL, one file per run.
//!
//! The plan's reason for this being the source of truth is specific to the
//! target hardware: "Pi-class devices may get killed by OOM or power loss", so
//! replay and resumability matter more here than they do for a cloud harness.
//! Two consequences shape this module:
//!
//! * **`fsync` after every append.** An event that is in the page cache when the
//!   power drops did not happen, and on a runbook that restarts services that is
//!   the difference between "did I already restart it" and "restart it again".
//!   The cost is real on SD cards, so [`Durability`] makes it switchable — but
//!   the default is the safe one.
//! * **A truncated final line is expected, not corruption.** Losing power
//!   mid-`write` leaves a partial JSON object. Replay drops a bad *final* line
//!   and reports it; a bad line anywhere else is genuine corruption and is an
//!   error, because silently skipping it would desynchronize the sequence
//!   numbers that make replay meaningful.

use crate::action::{fingerprint, Action};
use crate::validate::Reject;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// The structured failure record. Deliberately **not** a prose string: §3
/// requires every `Error` event to be retrievable later by tool, by argument
/// similarity, and by task context, and you cannot query a sentence. Every field
/// here exists because the failure cache will need to match on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorRecord {
    /// Runbook step id, for task-context similarity.
    pub step_id: String,
    /// Step intent text — the natural-language side of the similarity query.
    pub step_intent: String,
    pub tool: Option<String>,
    /// Stable code from `RejectCode::as_str`, for exact-match retrieval.
    pub code: String,
    /// O(1) "have I tried this exact call before" key.
    pub args_fingerprint: Option<String>,
    /// Operator-facing explanation.
    pub reason: String,
    /// Model-facing correction. This is the text the failure cache replays.
    pub repair_hint: String,
}

impl ErrorRecord {
    pub fn from_reject(step_id: &str, step_intent: &str, action: Option<&Action>, r: &Reject) -> Self {
        ErrorRecord {
            step_id: step_id.to_string(),
            step_intent: step_intent.to_string(),
            tool: action.map(|a| a.tool.as_str().to_string()),
            code: r.code.as_str().to_string(),
            args_fingerprint: action.map(fingerprint),
            reason: r.reason.clone(),
            repair_hint: r.repair_hint.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventKind {
    UserMessage {
        text: String,
    },
    AgentThought {
        text: String,
    },
    /// `raw` is kept alongside the parsed action because Phase 0's headline
    /// metric is how often the model emits something invalid — which is
    /// unmeasurable if the harness only records what parsed.
    ActionProposed {
        step_id: String,
        raw: String,
        action: Option<Action>,
    },
    ActionExecuted {
        step_id: String,
        action: Action,
    },
    Observation {
        step_id: String,
        exit_code: Option<i32>,
        text: String,
    },
    Error(ErrorRecord),
    /// Compressed resumption point (plan §3.5's scratchpad).
    StateSnapshot {
        step_idx: usize,
        scratchpad: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub seq: u64,
    pub ts_ms: u64,
    #[serde(flatten)]
    pub kind: EventKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Durability {
    /// `fsync` after every append. Default.
    Sync,
    /// Let the OS flush. Faster on SD cards; loses the tail on power failure.
    Relaxed,
}

#[derive(Debug)]
pub enum LogError {
    Io(std::io::Error),
    /// A malformed line that is not the final line.
    Corrupt { line_no: usize, detail: String },
    /// Sequence numbers are not contiguous and ascending from 1.
    SequenceBreak { expected: u64, found: u64 },
}

impl std::fmt::Display for LogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LogError::Io(e) => write!(f, "io error: {}", e),
            LogError::Corrupt { line_no, detail } => {
                write!(f, "corrupt event at line {}: {}", line_no, detail)
            }
            LogError::SequenceBreak { expected, found } => {
                write!(f, "sequence break: expected {}, found {}", expected, found)
            }
        }
    }
}

impl From<std::io::Error> for LogError {
    fn from(e: std::io::Error) -> Self {
        LogError::Io(e)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replay {
    pub events: Vec<Event>,
    /// True if an incomplete final line was discarded — i.e. the previous run
    /// almost certainly died mid-write. Surfaced so the agent loop can say so
    /// rather than resuming from a state nobody knows is short.
    pub dropped_partial_tail: bool,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub struct EventLog {
    path: PathBuf,
    file: File,
    next_seq: u64,
    durability: Durability,
}

impl EventLog {
    /// Open (creating if absent) and position after the last intact event.
    pub fn open(path: impl AsRef<Path>, durability: Durability) -> Result<(EventLog, Replay), LogError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let replay = if path.exists() {
            Self::replay(&path)?
        } else {
            Replay {
                events: vec![],
                dropped_partial_tail: false,
            }
        };

        // A partial tail is rewritten away, so the next append does not land
        // after half a record and turn a recoverable log into a corrupt one.
        if replay.dropped_partial_tail {
            let mut buf = String::new();
            for e in &replay.events {
                buf.push_str(&serde_json::to_string(e).expect("event must serialize"));
                buf.push('\n');
            }
            std::fs::write(&path, buf)?;
        }

        let next_seq = replay.events.last().map(|e| e.seq + 1).unwrap_or(1);
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok((
            EventLog {
                path,
                file,
                next_seq,
                durability,
            },
            replay,
        ))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    pub fn append(&mut self, kind: EventKind) -> Result<Event, LogError> {
        let ev = Event {
            seq: self.next_seq,
            ts_ms: now_ms(),
            kind,
        };
        let line = serde_json::to_string(&ev).expect("event must serialize");
        // One write call for line+newline: a partial write then shows up as a
        // truncated final line, which replay handles, rather than a valid line
        // with no terminator.
        self.file.write_all(format!("{}\n", line).as_bytes())?;
        if self.durability == Durability::Sync {
            self.file.sync_data()?;
        }
        self.next_seq += 1;
        Ok(ev)
    }

    /// Convenience: log a rejection as a structured `Error` event.
    pub fn append_reject(
        &mut self,
        step_id: &str,
        step_intent: &str,
        action: Option<&Action>,
        r: &Reject,
    ) -> Result<Event, LogError> {
        self.append(EventKind::Error(ErrorRecord::from_reject(
            step_id, step_intent, action, r,
        )))
    }

    pub fn replay(path: impl AsRef<Path>) -> Result<Replay, LogError> {
        let f = File::open(path.as_ref())?;
        let reader = BufReader::new(f);
        let lines: Vec<String> = reader.lines().collect::<Result<_, _>>()?;

        let mut events = vec![];
        let mut dropped_partial_tail = false;
        let total = lines.len();

        for (i, line) in lines.into_iter().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Event>(&line) {
                Ok(ev) => {
                    let expected = events.last().map(|e: &Event| e.seq + 1).unwrap_or(1);
                    if ev.seq != expected {
                        return Err(LogError::SequenceBreak {
                            expected,
                            found: ev.seq,
                        });
                    }
                    events.push(ev);
                }
                Err(e) => {
                    if i + 1 == total {
                        // Expected shape of a power-loss death.
                        dropped_partial_tail = true;
                    } else {
                        return Err(LogError::Corrupt {
                            line_no: i + 1,
                            detail: e.to_string(),
                        });
                    }
                }
            }
        }

        Ok(Replay {
            events,
            dropped_partial_tail,
        })
    }
}

/// Every `Error` event in the stream. This is the query surface the failure
/// cache (§3) is built on; it lives here so that the cache is an *index over the
/// log* rather than a second, separately-writable source of truth that can drift.
pub fn error_records(events: &[Event]) -> Vec<&ErrorRecord> {
    events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::Error(r) => Some(r),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::parse_action;
    use crate::validate::{validate, RejectCode};
    use crate::runbook::example_service_restart;

    fn tmpdir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("mpodol-test-{}-{}", tag, std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn event_log_roundtrip() {
        let path = tmpdir("roundtrip").join("events.jsonl");
        let _ = std::fs::remove_file(&path);

        let (mut log, replay) = EventLog::open(&path, Durability::Sync).unwrap();
        assert!(replay.events.is_empty());
        assert_eq!(log.next_seq(), 1);

        log.append(EventKind::UserMessage {
            text: "restart telemetry".into(),
        })
        .unwrap();
        log.append(EventKind::AgentThought {
            text: "read config first".into(),
        })
        .unwrap();
        let a = parse_action(r#"{"tool":"file_read","args":{"path":"/etc/mpodol/t.conf"}}"#).unwrap();
        log.append(EventKind::ActionExecuted {
            step_id: "read-config".into(),
            action: a.clone(),
        })
        .unwrap();

        let back = EventLog::replay(&path).unwrap();
        assert!(!back.dropped_partial_tail);
        assert_eq!(back.events.len(), 3);
        assert_eq!(back.events.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![1, 2, 3]);
        match &back.events[2].kind {
            EventKind::ActionExecuted { action, step_id } => {
                assert_eq!(action, &a);
                assert_eq!(step_id, "read-config");
            }
            other => panic!("wrong kind: {:?}", other),
        }
    }

    #[test]
    fn reopening_continues_the_sequence() {
        let path = tmpdir("reopen").join("events.jsonl");
        let _ = std::fs::remove_file(&path);
        {
            let (mut log, _) = EventLog::open(&path, Durability::Sync).unwrap();
            log.append(EventKind::UserMessage { text: "a".into() }).unwrap();
        }
        let (mut log, replay) = EventLog::open(&path, Durability::Sync).unwrap();
        assert_eq!(replay.events.len(), 1);
        assert_eq!(log.next_seq(), 2);
        let ev = log.append(EventKind::UserMessage { text: "b".into() }).unwrap();
        assert_eq!(ev.seq, 2);
    }

    #[test]
    fn event_log_survives_truncated_tail() {
        // Simulates power loss mid-write on a Pi.
        let path = tmpdir("tail").join("events.jsonl");
        let _ = std::fs::remove_file(&path);
        {
            let (mut log, _) = EventLog::open(&path, Durability::Sync).unwrap();
            log.append(EventKind::UserMessage { text: "one".into() }).unwrap();
            log.append(EventKind::UserMessage { text: "two".into() }).unwrap();
        }
        let mut raw = std::fs::read_to_string(&path).unwrap();
        raw.push_str("{\"seq\":3,\"ts_ms\":1,\"kind\":\"user_mes");
        std::fs::write(&path, raw).unwrap();

        let (mut log, replay) = EventLog::open(&path, Durability::Sync).unwrap();
        assert!(replay.dropped_partial_tail, "partial tail must be reported");
        assert_eq!(replay.events.len(), 2);
        assert_eq!(log.next_seq(), 3);

        // And the log must be appendable again without producing a file that no
        // longer replays.
        log.append(EventKind::UserMessage { text: "three".into() }).unwrap();
        let back = EventLog::replay(&path).unwrap();
        assert!(!back.dropped_partial_tail);
        assert_eq!(back.events.len(), 3);
    }

    #[test]
    fn corruption_in_the_middle_is_an_error_not_a_skip() {
        let path = tmpdir("corrupt").join("events.jsonl");
        let _ = std::fs::remove_file(&path);
        {
            let (mut log, _) = EventLog::open(&path, Durability::Sync).unwrap();
            log.append(EventKind::UserMessage { text: "one".into() }).unwrap();
            log.append(EventKind::UserMessage { text: "two".into() }).unwrap();
        }
        let raw = std::fs::read_to_string(&path).unwrap();
        let mut lines: Vec<&str> = raw.lines().collect();
        lines.insert(1, "{ this is not an event }");
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();

        match EventLog::replay(&path) {
            Err(LogError::Corrupt { line_no, .. }) => assert_eq!(line_no, 2),
            other => panic!("expected Corrupt, got {:?}", other),
        }
    }

    #[test]
    fn rejects_are_logged_as_structured_records() {
        let path = tmpdir("reject").join("events.jsonl");
        let _ = std::fs::remove_file(&path);
        let rb = example_service_restart();
        let step = rb.step(0).unwrap();
        let action =
            parse_action(r#"{"tool":"file_read","args":{"path":"/etc/mpodol/../../etc/passwd"}}"#)
                .unwrap();
        let rej = validate(&rb, 0, &action).unwrap_err();

        let (mut log, _) = EventLog::open(&path, Durability::Sync).unwrap();
        log.append_reject(&step.id, &step.intent, Some(&action), &rej).unwrap();

        let back = EventLog::replay(&path).unwrap();
        let errs = error_records(&back.events);
        assert_eq!(errs.len(), 1);
        let r = errs[0];
        // Every field the failure cache will retrieve on must survive the trip.
        assert_eq!(r.code, RejectCode::PathOutsideRoot.as_str());
        assert_eq!(r.tool.as_deref(), Some("file_read"));
        assert_eq!(r.step_id, "read-config");
        assert!(r.args_fingerprint.is_some());
        assert!(!r.repair_hint.is_empty());
    }

    #[test]
    fn relaxed_durability_still_replays() {
        let path = tmpdir("relaxed").join("events.jsonl");
        let _ = std::fs::remove_file(&path);
        {
            let (mut log, _) = EventLog::open(&path, Durability::Relaxed).unwrap();
            for i in 0..50 {
                log.append(EventKind::AgentThought {
                    text: format!("t{}", i),
                })
                .unwrap();
            }
        }
        assert_eq!(EventLog::replay(&path).unwrap().events.len(), 50);
    }
}
