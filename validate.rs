//! Deterministic validation (plan §3.3, directive §5 innovation 2).
//!
//! Two passes, run in order, deliberately **not** sharing enforcement code with
//! `grammar.rs`:
//!
//! 1. [`validate_syntactic`] — structure only. Are the declared arguments
//!    present, are their types right, is anything extra or malformed? Depends on
//!    the tool, not on the runbook.
//! 2. [`validate_semantic`] — the step contract. Is this tool authorized *here*,
//!    do its argument values stay inside the bounds the operator declared, is the
//!    runbook being walked in order?
//!
//! # Why this is a separate pass and not a wrapper around the grammar
//!
//! The directive is blunt that most of the reliability work has to live here,
//! and the reason is structural rather than stylistic: **a grammar is a shape
//! over tokens and cannot evaluate.** The generated path rule bakes
//! `/etc/mpodol/` in as a literal prefix, which is genuinely strong — and
//! `/etc/mpodol/../../etc/passwd` satisfies it exactly while resolving somewhere
//! else entirely. No context-free grammar can normalize a path. That case alone
//! establishes that this pass is load-bearing rather than belt-and-braces.
//!
//! There is a second, sharper reason to keep the layers independent: **the
//! grammar is a reliability device and must never be load-bearing for safety.**
//! It is switchable off (`--no-grammar`, useful for measuring what it buys), it
//! does not exist for non-llama.cpp backends, and it only constrains sampling —
//! an action replayed from a log or injected by a future API arrives having
//! never passed through it. So this pass re-checks even the things the grammar
//! already made unrepresentable, including the tool name. If the two layers
//! shared code, switching one off would silently weaken the other.

use crate::action::{Action, ArgKind, ArgValue};
use crate::runbook::{Permit, Runbook};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectCode {
    // syntactic
    MissingArg,
    UnknownArg,
    ArgType,
    ControlCharacter,
    // semantic
    StepUnknown,
    ToolNotPermittedAtStep,
    StepOutOfOrder,
    PathNotAbsolute,
    PathAboveFilesystemRoot,
    PathOutsideRoot,
    Argv0NotAllowed,
    ShellMetacharacter,
    HostNotAllowed,
    UrlMalformed,
    ArgTooLarge,
    TimeoutAboveCeiling,
}

impl RejectCode {
    /// Stable string form. Used as the failure-cache retrieval key (§3), so it
    /// must not be derived from the Debug impl — renaming a variant would
    /// silently orphan every cached failure.
    pub fn as_str(self) -> &'static str {
        match self {
            RejectCode::MissingArg => "missing_arg",
            RejectCode::UnknownArg => "unknown_arg",
            RejectCode::ArgType => "arg_type",
            RejectCode::ControlCharacter => "control_character",
            RejectCode::StepUnknown => "step_unknown",
            RejectCode::ToolNotPermittedAtStep => "tool_not_permitted_at_step",
            RejectCode::StepOutOfOrder => "step_out_of_order",
            RejectCode::PathNotAbsolute => "path_not_absolute",
            RejectCode::PathAboveFilesystemRoot => "path_above_filesystem_root",
            RejectCode::PathOutsideRoot => "path_outside_root",
            RejectCode::Argv0NotAllowed => "argv0_not_allowed",
            RejectCode::ShellMetacharacter => "shell_metacharacter",
            RejectCode::HostNotAllowed => "host_not_allowed",
            RejectCode::UrlMalformed => "url_malformed",
            RejectCode::ArgTooLarge => "arg_too_large",
            RejectCode::TimeoutAboveCeiling => "timeout_above_ceiling",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reject {
    pub code: RejectCode,
    /// For the operator and the audit log.
    pub reason: String,
    /// For the model. Concrete and actionable — this is what the repair loop
    /// re-prompts with, and what the failure cache replays as "last time this
    /// was tried in a similar context, it failed because X".
    pub repair_hint: String,
}

impl Reject {
    fn new(code: RejectCode, reason: String, repair_hint: String) -> Reject {
        Reject {
            code,
            reason,
            repair_hint,
        }
    }
}

impl std::fmt::Display for Reject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.code.as_str(), self.reason)
    }
}

/// Characters that only matter if a shell interprets the command line. The
/// runtime execs `argv` directly with no shell, so these can never do what the
/// model intends — their presence means either an attempt to chain commands past
/// the argv0 allowlist, or a model that has confused the tool for a shell
/// prompt. Both are worth a turn to correct rather than silently mangling.
const SHELL_METACHARS: &[char] = &[';', '|', '&', '`', '$', '>', '<', '(', ')', '\n', '\r'];

/// Pass 1: structure. Tool-dependent, runbook-independent.
pub fn validate_syntactic(action: &Action) -> Result<(), Reject> {
    let specs = action.tool.args();

    for spec in specs {
        let got = action.args.get(spec.name).ok_or_else(|| {
            Reject::new(
                RejectCode::MissingArg,
                format!(
                    "tool {} requires argument {:?}",
                    action.tool.as_str(),
                    spec.name
                ),
                format!(
                    "Include the argument \"{}\" for tool \"{}\". Required arguments, in order: {}.",
                    spec.name,
                    action.tool.as_str(),
                    specs.iter().map(|s| s.name).collect::<Vec<_>>().join(", ")
                ),
            )
        })?;

        let type_ok = match spec.kind {
            ArgKind::Path | ArgKind::Text | ArgKind::Shell | ArgKind::Url => {
                matches!(got, ArgValue::Str(_))
            }
            ArgKind::Int => matches!(got, ArgValue::Int(_)),
        };
        if !type_ok {
            return Err(Reject::new(
                RejectCode::ArgType,
                format!(
                    "argument {:?} of {} has the wrong type",
                    spec.name,
                    action.tool.as_str()
                ),
                match spec.kind {
                    ArgKind::Int => format!("Argument \"{}\" must be a whole number, not a string.", spec.name),
                    _ => format!("Argument \"{}\" must be a quoted string.", spec.name),
                },
            ));
        }

        // The grammar's `char` class excludes only `"` and `\` (see grammar.rs
        // on avoiding \x escapes for compatibility), so control characters are
        // samplable and have to be caught here.
        if let ArgValue::Str(s) = got {
            if let Some(c) = s.chars().find(|c| c.is_control()) {
                return Err(Reject::new(
                    RejectCode::ControlCharacter,
                    format!(
                        "argument {:?} contains control character U+{:04X}",
                        spec.name, c as u32
                    ),
                    format!("Argument \"{}\" must not contain control characters or newlines.", spec.name),
                ));
            }
        }
    }

    for k in action.args.keys() {
        if !specs.iter().any(|s| s.name == k) {
            return Err(Reject::new(
                RejectCode::UnknownArg,
                format!(
                    "tool {} has no argument {:?}",
                    action.tool.as_str(),
                    k
                ),
                format!(
                    "Remove \"{}\". Tool \"{}\" takes exactly: {}.",
                    k,
                    action.tool.as_str(),
                    specs.iter().map(|s| s.name).collect::<Vec<_>>().join(", ")
                ),
            ));
        }
    }

    Ok(())
}

/// Lexically normalize an absolute path: resolve `.` and `..` textually, without
/// touching the filesystem.
///
/// Purely lexical on purpose — the target may not exist yet (`file_write`), and
/// `canonicalize()` would both fail on missing paths and hit the disk on every
/// validation. Returns `None` if the path is relative or climbs above `/`.
///
/// **This is necessary but not sufficient.** Lexical normalization cannot see a
/// symlink: `/etc/mpodol/evil -> /etc/shadow` normalizes to a path inside the
/// root and resolves outside it. Closing that hole belongs to the runtime, which
/// must open with `openat2(RESOLVE_BENEATH)` (or `O_NOFOLLOW` per component) so
/// the kernel enforces containment. Tracked in docs/open-issues.md — the
/// sandbox is not written yet, and this comment exists so the gap is not
/// rediscovered as a surprise later.
pub fn normalize_lexical(path: &str) -> Option<String> {
    if !path.starts_with('/') {
        return None;
    }
    let mut out: Vec<&str> = vec![];
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if out.pop().is_none() {
                    return None;
                }
            }
            s => out.push(s),
        }
    }
    Some(format!("/{}", out.join("/")))
}

/// Is `path` at or below `root`, both already normalized?
///
/// Compares on segment boundaries. A naive `starts_with` would accept
/// `/etc/mpodolicious/secrets` for the root `/etc/mpodol` — a real and easy bug,
/// which `path_root_check_respects_segment_boundaries` pins down.
fn is_under(path: &str, root: &str) -> bool {
    if root == "/" {
        return true;
    }
    path == root || path.starts_with(&format!("{}/", root))
}

fn check_path(arg: &str, value: &str, permit: &Permit) -> Result<(), Reject> {
    if !value.starts_with('/') {
        return Err(Reject::new(
            RejectCode::PathNotAbsolute,
            format!("argument {:?} is not an absolute path: {:?}", arg, value),
            format!("Argument \"{}\" must be an absolute path starting with '/'.", arg),
        ));
    }

    let norm = normalize_lexical(value).ok_or_else(|| {
        Reject::new(
            RejectCode::PathAboveFilesystemRoot,
            format!("path {:?} climbs above the filesystem root", value),
            format!("Argument \"{}\" contains too many '..' segments.", arg),
        )
    })?;

    if permit.path_roots.is_empty() {
        return Ok(());
    }

    let roots: Vec<String> = permit
        .path_roots
        .iter()
        .filter_map(|r| normalize_lexical(r))
        .collect();

    if roots.iter().any(|r| is_under(&norm, r)) {
        return Ok(());
    }

    let traversed = norm != value.trim_end_matches('/') && value.contains("..");
    Err(Reject::new(
        RejectCode::PathOutsideRoot,
        format!(
            "path {:?} resolves to {:?}, which is outside the permitted root(s) {:?}",
            value, norm, permit.path_roots
        ),
        if traversed {
            format!(
                "Argument \"{}\" uses '..' to leave the permitted directory: {:?} resolves to {:?}. Stay within {}.",
                arg, value, norm, permit.path_roots.join(", ")
            )
        } else {
            format!(
                "Argument \"{}\" must be a path under {}.",
                arg,
                permit.path_roots.join(", ")
            )
        },
    ))
}

fn check_shell(arg: &str, value: &str, permit: &Permit) -> Result<(), Reject> {
    if let Some(c) = value.chars().find(|c| SHELL_METACHARS.contains(c)) {
        return Err(Reject::new(
            RejectCode::ShellMetacharacter,
            format!("command contains shell metacharacter {:?}: {:?}", c, value),
            format!(
                "Argument \"{}\" is executed directly, not through a shell, so {:?} cannot work. Issue one command with plain arguments; chaining, pipes and redirection are not available.",
                arg, c
            ),
        ));
    }

    let argv0 = value.split_whitespace().next().unwrap_or("");
    if argv0.is_empty() {
        return Err(Reject::new(
            RejectCode::Argv0NotAllowed,
            "command is empty".into(),
            format!("Argument \"{}\" must name a command to run.", arg),
        ));
    }

    if permit.argv0_allow.is_empty() {
        return Ok(());
    }
    if permit.argv0_allow.iter().any(|a| a == argv0) {
        return Ok(());
    }
    Err(Reject::new(
        RejectCode::Argv0NotAllowed,
        format!(
            "command {:?} is not in this step's allowlist {:?}",
            argv0, permit.argv0_allow
        ),
        format!(
            "At this step the only permitted command(s) are: {}.",
            permit.argv0_allow.join(", ")
        ),
    ))
}

fn check_url(arg: &str, value: &str, permit: &Permit) -> Result<(), Reject> {
    let rest = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
        .ok_or_else(|| {
            Reject::new(
                RejectCode::UrlMalformed,
                format!("url {:?} has no http(s) scheme", value),
                format!("Argument \"{}\" must start with http:// or https://.", arg),
            )
        })?;
    let host = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .split('@')
        .last()
        .unwrap_or("");
    if host.is_empty() {
        return Err(Reject::new(
            RejectCode::UrlMalformed,
            format!("url {:?} has no host", value),
            format!("Argument \"{}\" must include a hostname.", arg),
        ));
    }
    if permit.host_allow.is_empty() {
        return Ok(());
    }
    let bare = host.split(':').next().unwrap_or(host);
    if permit.host_allow.iter().any(|h| h == bare) {
        return Ok(());
    }
    Err(Reject::new(
        RejectCode::HostNotAllowed,
        format!(
            "host {:?} is not in this step's allowlist {:?}",
            bare, permit.host_allow
        ),
        format!(
            "At this step only these hosts may be fetched: {}.",
            permit.host_allow.join(", ")
        ),
    ))
}

/// Pass 2: the step contract.
pub fn validate_semantic(rb: &Runbook, step_idx: usize, action: &Action) -> Result<(), Reject> {
    let step = rb.step(step_idx).ok_or_else(|| {
        Reject::new(
            RejectCode::StepUnknown,
            format!("step index {} is past the end of runbook {:?}", step_idx, rb.name),
            "The runbook has no further steps.".into(),
        )
    })?;

    // Re-check the tool name even though the grammar made other tools
    // unspellable — see the module header on why the layers stay independent.
    let permit = match step.permit_for(action.tool) {
        Some(p) => p,
        None => {
            // Distinguish "never allowed" from "not yet": a model reaching for
            // step 3's tool during step 1 has understood the task and mis-ordered
            // it, which is a different failure needing a different hint. The
            // failure cache (§3) also wants these kept apart, so that a genuine
            // ordering mistake is not retrieved as evidence a tool is forbidden.
            if let Some(later) = rb.next_step_permitting(action.tool, step_idx + 1) {
                let later_step = rb.step(later).unwrap();
                return Err(Reject::new(
                    RejectCode::StepOutOfOrder,
                    format!(
                        "tool {} belongs to step {:?} (index {}), not the current step {:?} (index {})",
                        action.tool.as_str(),
                        later_step.id,
                        later,
                        step.id,
                        step_idx
                    ),
                    format!(
                        "That comes later. The current step is {:?}: {} Permitted now: {}.",
                        step.id,
                        step.intent,
                        step.permits
                            .iter()
                            .map(|p| p.tool.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                ));
            }
            return Err(Reject::new(
                RejectCode::ToolNotPermittedAtStep,
                format!(
                    "tool {} is not permitted at step {:?} and appears nowhere later in the runbook",
                    action.tool.as_str(),
                    step.id
                ),
                format!(
                    "Tool \"{}\" is not available in this runbook. The current step {:?} permits: {}.",
                    action.tool.as_str(),
                    step.id,
                    step.permits
                        .iter()
                        .map(|p| p.tool.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ));
        }
    };

    for spec in action.tool.args() {
        // Presence and type are pass 1's job; by here they hold.
        let Some(v) = action.args.get(spec.name) else {
            continue;
        };
        match (spec.kind, v) {
            (ArgKind::Path, ArgValue::Str(s)) => check_path(spec.name, s, permit)?,
            (ArgKind::Shell, ArgValue::Str(s)) => check_shell(spec.name, s, permit)?,
            (ArgKind::Url, ArgValue::Str(s)) => check_url(spec.name, s, permit)?,
            (ArgKind::Text, ArgValue::Str(s)) => {
                if let Some(cap) = permit.max_text_bytes {
                    if s.len() > cap {
                        return Err(Reject::new(
                            RejectCode::ArgTooLarge,
                            format!(
                                "argument {:?} is {} bytes, over this step's cap of {}",
                                spec.name,
                                s.len(),
                                cap
                            ),
                            format!(
                                "Argument \"{}\" must be at most {} bytes at this step; it was {}.",
                                spec.name, cap, s.len()
                            ),
                        ));
                    }
                }
            }
            (ArgKind::Int, ArgValue::Int(n)) => {
                if spec.name == "timeout_ms" {
                    if let Some(cap) = permit.max_timeout_ms {
                        if *n > cap {
                            // The grammar bounds the digit *count*, which cannot
                            // express a ceiling of 5000 — a 4-digit bound still
                            // admits 9999. This is the numeric twin of the path
                            // traversal case.
                            return Err(Reject::new(
                                RejectCode::TimeoutAboveCeiling,
                                format!("timeout_ms {} exceeds this step's ceiling {}", n, cap),
                                format!("Set \"timeout_ms\" to at most {}.", cap),
                            ));
                        }
                    }
                }
            }
            _ => {}
        }
    }

    Ok(())
}

/// Both passes, in order. The only entry point the agent loop should call.
pub fn validate(rb: &Runbook, step_idx: usize, action: &Action) -> Result<(), Reject> {
    validate_syntactic(action)?;
    validate_semantic(rb, step_idx, action)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::{parse_action, Tool};
    use crate::grammar::{compile_step, grammar_admits_path};
    use crate::runbook::example_service_restart;

    fn act(s: &str) -> Action {
        parse_action(s).expect("test fixture must parse")
    }

    // ---- the test that keeps the validator honest -------------------------
    // A validator that rejects everything passes every negative test below.
    #[test]
    fn semantic_accepts_intended_action() {
        let rb = example_service_restart();
        validate(
            &rb,
            0,
            &act(r#"{"tool":"file_read","args":{"path":"/etc/mpodol/telemetry.conf"}}"#),
        )
        .expect("the happy path must pass");

        validate(
            &rb,
            1,
            &act(r#"{"tool":"shell_exec","args":{"cmd":"systemctl restart telemetry-agent","timeout_ms":4000}}"#),
        )
        .expect("the happy path must pass");

        validate(
            &rb,
            2,
            &act(r#"{"tool":"file_write","args":{"path":"/var/log/mpodol/run.log","content":"ok"}}"#),
        )
        .expect("the happy path must pass");
    }

    // ---- the case that proves both layers are load-bearing ----------------
    #[test]
    fn semantic_rejects_traversal_that_grammar_admits() {
        let rb = example_service_restart();
        let step = rb.step(0).unwrap();
        let permit = step.permit_for(Tool::FileRead).unwrap();
        let sneaky = "/etc/mpodol/../../etc/passwd";

        // Half 1: the grammar admits it. The baked-in root prefix matches
        // exactly, and every remaining character is in the `char` class.
        let g = compile_step(step).unwrap();
        assert!(g.gbnf.contains(r#""\"/etc/mpodol/""#));
        assert!(
            grammar_admits_path(permit, sneaky),
            "grammar should admit this path shape — that is the whole point"
        );

        // Half 2: the semantic pass rejects it anyway.
        let err = validate(
            &rb,
            0,
            &act(r#"{"tool":"file_read","args":{"path":"/etc/mpodol/../../etc/passwd"}}"#),
        )
        .unwrap_err();
        assert_eq!(err.code, RejectCode::PathOutsideRoot);
        assert!(err.reason.contains("/etc/passwd"), "reason: {}", err.reason);
        // The hint has to name the mechanism, or the repair loop teaches nothing.
        assert!(err.repair_hint.contains(".."), "hint: {}", err.repair_hint);
    }

    #[test]
    fn semantic_rejects_wrong_tool_independently_of_grammar() {
        // Nothing here consults the grammar: an action arriving from a replayed
        // log or a future API never passed through sampling at all.
        let rb = example_service_restart();
        let err = validate(
            &rb,
            0,
            &act(r#"{"tool":"http_fetch","args":{"url":"https://example.com/x"}}"#),
        )
        .unwrap_err();
        assert_eq!(err.code, RejectCode::ToolNotPermittedAtStep);
    }

    #[test]
    fn out_of_order_is_distinguished_from_forbidden() {
        let rb = example_service_restart();
        // shell_exec is legitimate, but at step 1, not step 0.
        let err = validate(
            &rb,
            0,
            &act(r#"{"tool":"shell_exec","args":{"cmd":"systemctl restart x","timeout_ms":100}}"#),
        )
        .unwrap_err();
        assert_eq!(err.code, RejectCode::StepOutOfOrder);
        assert!(err.repair_hint.contains("comes later"));
    }

    #[test]
    fn shell_chaining_is_rejected_even_with_allowed_argv0() {
        // The argv0 allowlist is worthless on its own: `systemctl` is permitted,
        // and the grammar's `char*` tail happily admits `; rm -rf /`.
        let rb = example_service_restart();
        let err = validate(
            &rb,
            1,
            &act(r#"{"tool":"shell_exec","args":{"cmd":"systemctl restart x; rm -rf /","timeout_ms":100}}"#),
        )
        .unwrap_err();
        assert_eq!(err.code, RejectCode::ShellMetacharacter);
    }

    #[test]
    fn disallowed_argv0_is_rejected() {
        let rb = example_service_restart();
        let err = validate(
            &rb,
            1,
            &act(r#"{"tool":"shell_exec","args":{"cmd":"reboot now","timeout_ms":100}}"#),
        )
        .unwrap_err();
        assert_eq!(err.code, RejectCode::Argv0NotAllowed);
        assert!(err.repair_hint.contains("systemctl"));
    }

    #[test]
    fn timeout_ceiling_is_enforced_beyond_the_digit_bound() {
        // 9999 has four digits, so the grammar admits it; the ceiling is 5000.
        let rb = example_service_restart();
        let err = validate(
            &rb,
            1,
            &act(r#"{"tool":"shell_exec","args":{"cmd":"systemctl restart x","timeout_ms":9999}}"#),
        )
        .unwrap_err();
        assert_eq!(err.code, RejectCode::TimeoutAboveCeiling);
    }

    #[test]
    fn text_byte_cap_is_enforced() {
        let rb = example_service_restart();
        let big = "x".repeat(4097);
        let a = act(&format!(
            r#"{{"tool":"file_write","args":{{"path":"/var/log/mpodol/run.log","content":"{}"}}}}"#,
            big
        ));
        let err = validate(&rb, 2, &a).unwrap_err();
        assert_eq!(err.code, RejectCode::ArgTooLarge);
    }

    #[test]
    fn path_root_check_respects_segment_boundaries() {
        // `/etc/mpodolicious/` must not pass for the root `/etc/mpodol/`.
        let rb = example_service_restart();
        let err = validate(
            &rb,
            0,
            &act(r#"{"tool":"file_read","args":{"path":"/etc/mpodolicious/secrets"}}"#),
        )
        .unwrap_err();
        assert_eq!(err.code, RejectCode::PathOutsideRoot);
    }

    #[test]
    fn normalize_lexical_cases() {
        assert_eq!(normalize_lexical("/a/./b/../c").as_deref(), Some("/a/c"));
        assert_eq!(normalize_lexical("/a//b/").as_deref(), Some("/a/b"));
        assert_eq!(normalize_lexical("/..") , None);
        assert_eq!(normalize_lexical("/a/../.."), None);
        assert_eq!(normalize_lexical("a/b"), None);
        assert_eq!(normalize_lexical("/").as_deref(), Some("/"));
    }

    #[test]
    fn syntactic_catches_missing_and_extra_args() {
        let e = validate_syntactic(&act(r#"{"tool":"file_write","args":{"path":"/tmp/x"}}"#))
            .unwrap_err();
        assert_eq!(e.code, RejectCode::MissingArg);
        assert!(e.repair_hint.contains("content"));

        let e = validate_syntactic(&act(
            r#"{"tool":"file_read","args":{"path":"/tmp/x","mode":"rw"}}"#,
        ))
        .unwrap_err();
        assert_eq!(e.code, RejectCode::UnknownArg);
    }

    #[test]
    fn syntactic_catches_wrong_type() {
        let e = validate_syntactic(&act(
            r#"{"tool":"shell_exec","args":{"cmd":"ls","timeout_ms":"1000"}}"#,
        ))
        .unwrap_err();
        assert_eq!(e.code, RejectCode::ArgType);
        assert!(e.repair_hint.contains("whole number"));
    }

    #[test]
    fn syntactic_catches_control_characters_grammar_lets_through() {
        let mut a = act(r#"{"tool":"file_read","args":{"path":"/etc/mpodol/x"}}"#);
        a.args
            .insert("path".into(), ArgValue::Str("/etc/mpodol/x\u{0007}".into()));
        let e = validate_syntactic(&a).unwrap_err();
        assert_eq!(e.code, RejectCode::ControlCharacter);
    }

    #[test]
    fn step_past_end_of_runbook_is_rejected() {
        let rb = example_service_restart();
        let e = validate(&rb, 99, &act(r#"{"tool":"file_read","args":{"path":"/etc/mpodol/x"}}"#))
            .unwrap_err();
        assert_eq!(e.code, RejectCode::StepUnknown);
    }

    #[test]
    fn url_host_allowlist() {
        let rb = Runbook {
            name: "fetch".into(),
            steps: vec![crate::runbook::Step::new(
                "get",
                "fetch the manifest",
                vec![Permit::new(Tool::HttpFetch).hosts(["updates.local"])],
            )],
        };
        validate(
            &rb,
            0,
            &act(r#"{"tool":"http_fetch","args":{"url":"https://updates.local/manifest.json"}}"#),
        )
        .unwrap();

        let e = validate(
            &rb,
            0,
            &act(r#"{"tool":"http_fetch","args":{"url":"https://evil.example/x"}}"#),
        )
        .unwrap_err();
        assert_eq!(e.code, RejectCode::HostNotAllowed);

        // userinfo trick: the real host is evil.example, not updates.local.
        let e = validate(
            &rb,
            0,
            &act(r#"{"tool":"http_fetch","args":{"url":"https://updates.local@evil.example/x"}}"#),
        )
        .unwrap_err();
        assert_eq!(e.code, RejectCode::HostNotAllowed);
    }

    #[test]
    fn reject_codes_are_unique_strings() {
        let all = [
            RejectCode::MissingArg,
            RejectCode::UnknownArg,
            RejectCode::ArgType,
            RejectCode::ControlCharacter,
            RejectCode::StepUnknown,
            RejectCode::ToolNotPermittedAtStep,
            RejectCode::StepOutOfOrder,
            RejectCode::PathNotAbsolute,
            RejectCode::PathAboveFilesystemRoot,
            RejectCode::PathOutsideRoot,
            RejectCode::Argv0NotAllowed,
            RejectCode::ShellMetacharacter,
            RejectCode::HostNotAllowed,
            RejectCode::UrlMalformed,
            RejectCode::ArgTooLarge,
            RejectCode::TimeoutAboveCeiling,
        ];
        let mut seen: Vec<&str> = vec![];
        for c in all {
            assert!(!seen.contains(&c.as_str()), "duplicate code {}", c.as_str());
            seen.push(c.as_str());
        }
    }
}
