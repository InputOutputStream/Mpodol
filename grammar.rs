//! Step-scoped GBNF compiler (directive §5, innovation 1).
//!
//! # Why per-step and not one global grammar
//!
//! A global action grammar admits every tool at every moment. It buys valid
//! *syntax* and nothing else, so the model remains free to emit a well-formed
//! call to the wrong tool — and then the entire reliability burden falls on the
//! semantic pass, which can only reject after the fact and burn a turn. That is
//! the "constraint tax" the directive warns about: valid syntax, wrong
//! semantics, gains eaten.
//!
//! A step-scoped grammar makes whole classes of wrong action **unrepresentable**
//! at sampling time. At the `restart-service` step, no token sequence spelling
//! `file_write` exists. The model cannot pick it, cannot be re-prompted out of
//! it, and no turn is spent discovering it.
//!
//! # What we push down into the grammar, and what we deliberately do not
//!
//! Pushed down (cheap, total, and free at sampling time):
//!   * the tool name set for this step
//!   * key order and key spelling — emitted as literals, so no permutation and
//!     no misspelling is representable
//!   * path roots, baked in as **string literals**: at a step rooted in
//!     `/etc/mpodol/`, the grammar's path rule *starts* with that prefix, so no
//!     path outside it can be sampled
//!   * argv0 allowlists, as literal alternatives
//!   * digit-count bounds on integers
//!
//! Deliberately NOT pushed down, because a grammar is a shape over tokens and
//! cannot evaluate:
//!   * path normalization — `/etc/mpodol/../../etc/passwd` matches the baked-in
//!     prefix perfectly and still escapes the root. Only the semantic pass can
//!     catch it. This single case is the proof that neither layer is redundant.
//!   * numeric ceilings — a 4-digit bound admits `9999` when the ceiling is 5000
//!   * byte caps, step ordering, cross-argument consistency
//!
//! # Compatibility note
//!
//! Emission targets the conservative GBNF subset: literals, `|`, `*`, negated
//! character classes. It avoids `{m,n}` repetition (added to llama.cpp later
//! than some deployed builds) and `\x` escapes, generating explicit digit-count
//! alternatives instead. See `docs/open-issues.md` — validating output against
//! llama.cpp's own `gbnf-validator` in CI is an open task, and until it lands
//! this module's correctness rests on unit tests rather than on the real parser.

use crate::action::{ArgKind, Tool};
use crate::runbook::{Permit, Step};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrammarError {
    /// A literal destined for the grammar contains characters that cannot be
    /// safely embedded. Rejected at compile time rather than escaped, because a
    /// path root containing a quote is a sign of a malformed or hostile runbook,
    /// not something to normalize away.
    UnsafeLiteral { field: String, value: String },
    NoPermits { step_id: String },
}

impl std::fmt::Display for GrammarError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GrammarError::UnsafeLiteral { field, value } => write!(
                f,
                "value for {} cannot be embedded in a grammar literal: {:?}",
                field, value
            ),
            GrammarError::NoPermits { step_id } => {
                write!(f, "step {:?} permits no tools, so no grammar exists", step_id)
            }
        }
    }
}

/// A compiled grammar plus the warnings that came with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledGrammar {
    pub gbnf: String,
    /// Non-fatal: places where the operator left an argument unconstrained, so
    /// the semantic pass is the only remaining guard. Surfaced rather than
    /// swallowed — an unconstrained shell step should be a visible choice.
    pub warnings: Vec<String>,
}

/// Escape a Rust string into a GBNF double-quoted literal.
fn literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Reject literals we will not embed: control characters, quotes, backslashes.
fn check_safe(field: &str, value: &str) -> Result<(), GrammarError> {
    if value.chars().any(|c| c.is_control() || c == '"' || c == '\\') {
        return Err(GrammarError::UnsafeLiteral {
            field: field.to_string(),
            value: value.to_string(),
        });
    }
    Ok(())
}

/// Emit `n` alternatives of 1..=n digits, avoiding `{m,n}` repetition syntax.
fn digit_alternatives(max_digits: usize) -> String {
    (1..=max_digits.max(1))
        .map(|n| vec!["[0-9]"; n].join(" "))
        .collect::<Vec<_>>()
        .join(" | ")
}

fn digits_in(n: u64) -> usize {
    n.to_string().len()
}

/// Compile the grammar for a single runbook step.
pub fn compile_step(step: &Step) -> Result<CompiledGrammar, GrammarError> {
    if step.permits.is_empty() {
        return Err(GrammarError::NoPermits {
            step_id: step.id.clone(),
        });
    }

    let mut warnings = vec![];
    let mut alts: Vec<String> = vec![];
    let mut act_rules: Vec<String> = vec![];
    let mut arg_rules: Vec<String> = vec![];
    let mut needs_char = false;
    let mut needs_freestr = false;

    for (i, permit) in step.permits.iter().enumerate() {
        let tool = permit.tool;
        // Accumulate literal text, flushing it whenever a rule reference is
        // spliced in. This is what interleaves fixed JSON scaffolding with
        // variable argument rules.
        let mut parts: Vec<String> = vec![];
        let mut pending = String::new();
        let flush = |pending: &mut String, parts: &mut Vec<String>| {
            if !pending.is_empty() {
                parts.push(literal(pending));
                pending.clear();
            }
        };

        pending.push_str("{\"tool\":\"");
        pending.push_str(tool.as_str());
        pending.push_str("\",\"args\":{");

        for (j, arg) in tool.args().iter().enumerate() {
            if j > 0 {
                pending.push(',');
            }
            pending.push('"');
            pending.push_str(arg.name);
            pending.push_str("\":");

            let rule_name = format!("a{}-{}", i, arg.name.replace('_', "-"));

            match arg.kind {
                ArgKind::Path => {
                    if permit.path_roots.is_empty() {
                        warnings.push(format!(
                            "step {:?}: tool {} argument {:?} has no path_roots — the grammar cannot constrain it, so the semantic pass is the only guard",
                            step.id, tool.as_str(), arg.name
                        ));
                        flush(&mut pending, &mut parts);
                        parts.push("freestr".to_string());
                        needs_freestr = true;
                        needs_char = true;
                    } else {
                        let mut root_alts = vec![];
                        for root in &permit.path_roots {
                            check_safe("path_roots", root)?;
                            // Opening quote + root prefix are one literal, so the
                            // prefix is unskippable; the remainder is free chars.
                            root_alts.push(format!("{} char* \"\\\"\"", literal(&format!("\"{}", root))));
                        }
                        arg_rules.push(format!("{} ::= {}", rule_name, root_alts.join(" | ")));
                        needs_char = true;
                        flush(&mut pending, &mut parts);
                        parts.push(rule_name);
                    }
                }
                ArgKind::Shell => {
                    if permit.argv0_allow.is_empty() {
                        warnings.push(format!(
                            "step {:?}: tool {} argument {:?} has no argv0_allow — an unconstrained shell command is samplable",
                            step.id, tool.as_str(), arg.name
                        ));
                        flush(&mut pending, &mut parts);
                        parts.push("freestr".to_string());
                        needs_freestr = true;
                        needs_char = true;
                    } else {
                        let mut argv0_alts = vec![];
                        for a0 in &permit.argv0_allow {
                            check_safe("argv0_allow", a0)?;
                            // Two alternatives, so that argv0 is *anchored*:
                            // the command either ends there, or continues after
                            // a space. Without this, `"systemctl" char*` also
                            // admits `systemctlfoo` — the allowlist would
                            // constrain a prefix rather than a command. The
                            // semantic pass catches that too, but there is no
                            // reason to leave it samplable.
                            // Written as explicit alternatives rather than an
                            // optional group to stay inside the conservative
                            // GBNF subset (see module header).
                            argv0_alts.push(literal(&format!("\"{}\"", a0)));
                            argv0_alts
                                .push(format!("{} char* \"\\\"\"", literal(&format!("\"{} ", a0))));
                        }
                        arg_rules.push(format!("{} ::= {}", rule_name, argv0_alts.join(" | ")));
                        needs_char = true;
                        flush(&mut pending, &mut parts);
                        parts.push(rule_name);
                    }
                }
                ArgKind::Url => {
                    if permit.host_allow.is_empty() {
                        warnings.push(format!(
                            "step {:?}: tool {} argument {:?} has no host_allow",
                            step.id,
                            tool.as_str(),
                            arg.name
                        ));
                        flush(&mut pending, &mut parts);
                        parts.push("freestr".to_string());
                        needs_freestr = true;
                        needs_char = true;
                    } else {
                        let mut host_alts = vec![];
                        for h in &permit.host_allow {
                            check_safe("host_allow", h)?;
                            host_alts.push(format!(
                                "{} char* \"\\\"\"",
                                literal(&format!("\"http://{}/", h))
                            ));
                            host_alts.push(format!(
                                "{} char* \"\\\"\"",
                                literal(&format!("\"https://{}/", h))
                            ));
                        }
                        arg_rules.push(format!("{} ::= {}", rule_name, host_alts.join(" | ")));
                        needs_char = true;
                        flush(&mut pending, &mut parts);
                        parts.push(rule_name);
                    }
                }
                ArgKind::Text => {
                    flush(&mut pending, &mut parts);
                    parts.push("freestr".to_string());
                    needs_freestr = true;
                    needs_char = true;
                }
                ArgKind::Int => {
                    let max_digits = if arg.name == "timeout_ms" {
                        permit.max_timeout_ms.map(digits_in).unwrap_or(9)
                    } else {
                        9
                    };
                    arg_rules.push(format!(
                        "{} ::= {}",
                        rule_name,
                        digit_alternatives(max_digits)
                    ));
                    flush(&mut pending, &mut parts);
                    parts.push(rule_name);
                }
            }
        }

        pending.push_str("}}");
        flush(&mut pending, &mut parts);

        let alt_name = format!("act-{}", i);
        act_rules.push(format!("{} ::= {}", alt_name, parts.join(" ")));
        alts.push(alt_name);
    }

    let mut out = String::new();
    out.push_str(&format!(
        "# mpodol GBNF — step {:?}\n# intent: {}\n",
        step.id, step.intent
    ));
    out.push_str("# Generated per step. Do not hand-edit; edit the runbook's Permit instead.\n");
    out.push_str("# No whitespace productions: the model emits one compact JSON object,\n");
    out.push_str("# which removes indentation as a source of sampling divergence.\n\n");
    out.push_str(&format!("root ::= {}\n", alts.join(" | ")));
    for r in act_rules.iter().chain(arg_rules.iter()) {
        out.push_str(r);
        out.push('\n');
    }
    if needs_freestr {
        out.push_str("freestr ::= \"\\\"\" char* \"\\\"\"\n");
    }
    if needs_char {
        // Any character that does not terminate or escape a JSON string.
        // Control characters are excluded by the semantic pass, not here —
        // see the compatibility note above on avoiding \x escapes.
        out.push_str("char ::= [^\"\\\\]\n");
    }

    Ok(CompiledGrammar {
        gbnf: out,
        warnings,
    })
}

/// Which tool names are spellable under this grammar. Used by tests and by the
/// audit log — an operator reviewing a run should be able to see exactly what
/// the model was *able* to say at each step, not just what it did say.
pub fn spellable_tools(g: &CompiledGrammar) -> Vec<Tool> {
    Tool::ALL
        .iter()
        .copied()
        .filter(|t| g.gbnf.contains(&format!("\\\"tool\\\":\\\"{}", t.as_str())))
        .collect()
}

/// Test-only proxy for "would the grammar's path rule admit this string".
///
/// This is a **proxy, not a GBNF engine**: it encodes the one structural fact
/// the generated path rule guarantees (a baked-in root prefix followed by any
/// run of non-quote, non-backslash characters). It exists so that
/// `semantic_rejects_traversal_that_grammar_admits` can assert both halves of
/// its claim. Replacing it with real validation against llama.cpp's parser is
/// tracked in docs/open-issues.md.
pub fn grammar_admits_path(permit: &Permit, path: &str) -> bool {
    if path.contains('"') || path.contains('\\') {
        return false;
    }
    if permit.path_roots.is_empty() {
        return true;
    }
    permit.path_roots.iter().any(|r| path.starts_with(r.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runbook::example_service_restart;

    #[test]
    fn grammar_is_step_scoped() {
        let rb = example_service_restart();
        let g0 = compile_step(rb.step(0).unwrap()).unwrap();
        let g1 = compile_step(rb.step(1).unwrap()).unwrap();

        assert_ne!(g0.gbnf, g1.gbnf, "different steps must get different grammars");
        assert_eq!(spellable_tools(&g0), vec![Tool::FileRead]);
        assert_eq!(spellable_tools(&g1), vec![Tool::ShellExec]);

        // The point of the whole exercise: at step 1, "file_write" is not a
        // thing the model is able to say.
        assert!(!g1.gbnf.contains("file_write"));
        assert!(!g0.gbnf.contains("shell_exec"));
    }

    #[test]
    fn grammar_bakes_path_root_as_literal() {
        let rb = example_service_restart();
        let g = compile_step(rb.step(0).unwrap()).unwrap();
        // The opening JSON quote and the root travel together as one literal,
        // so the prefix cannot be skipped.
        assert!(
            g.gbnf.contains(r#""\"/etc/mpodol/""#),
            "root not baked in:\n{}",
            g.gbnf
        );
    }

    #[test]
    fn grammar_bakes_argv0_allowlist() {
        let rb = example_service_restart();
        let g = compile_step(rb.step(1).unwrap()).unwrap();
        let rule = g
            .gbnf
            .lines()
            .find(|l| l.starts_with("a0-cmd ::="))
            .expect("cmd rule must exist");
        // Exact rule text, not a substring search: the property under test is
        // that the allowlist contains *only* what the operator authorized, and a
        // loose `contains` check cannot see an extra alternative.
        assert_eq!(
            rule,
            r#"a0-cmd ::= "\"systemctl\"" | "\"systemctl " char* "\"""#
        );
    }

    #[test]
    fn argv0_is_anchored_not_merely_prefixed() {
        // `systemctlfoo` must not be spellable: argv0 either ends at the closing
        // quote or is followed by a space.
        let rb = example_service_restart();
        let g = compile_step(rb.step(1).unwrap()).unwrap();
        assert!(!g.gbnf.contains(r#""\"systemctl" char*"#), "argv0 left unanchored:\n{}", g.gbnf);
    }

    #[test]
    fn integer_digit_bound_tracks_the_ceiling() {
        let rb = example_service_restart();
        let g = compile_step(rb.step(1).unwrap()).unwrap();
        // max_timeout_ms = 5000 -> 4 digits max.
        assert!(g.gbnf.contains("[0-9] [0-9] [0-9] [0-9]"));
        assert!(!g.gbnf.contains("[0-9] [0-9] [0-9] [0-9] [0-9]"));
    }

    #[test]
    fn keys_are_emitted_in_declared_order() {
        let step = Step::new(
            "edit",
            "x",
            vec![Permit::new(Tool::FileEdit).roots(["/tmp/"])],
        );
        let g = compile_step(&step).unwrap();
        let p = g.gbnf.find("path").unwrap();
        let f = g.gbnf.find("find").unwrap();
        let r = g.gbnf.find("replace").unwrap();
        assert!(p < f && f < r, "declared arg order must survive into the grammar");
    }

    #[test]
    fn unconstrained_path_warns_rather_than_silently_widening() {
        let step = Step::new("s", "x", vec![Permit::new(Tool::FileRead)]);
        let g = compile_step(&step).unwrap();
        assert!(g.warnings.iter().any(|w| w.contains("no path_roots")));
    }

    #[test]
    fn unconstrained_shell_warns() {
        let step = Step::new("s", "x", vec![Permit::new(Tool::ShellExec)]);
        let g = compile_step(&step).unwrap();
        assert!(g.warnings.iter().any(|w| w.contains("no argv0_allow")));
    }

    #[test]
    fn hostile_literal_is_rejected_not_escaped() {
        let step = Step::new(
            "s",
            "x",
            vec![Permit::new(Tool::FileRead).roots(["/tmp/\"; rm -rf /"])],
        );
        match compile_step(&step) {
            Err(GrammarError::UnsafeLiteral { field, .. }) => assert_eq!(field, "path_roots"),
            other => panic!("expected UnsafeLiteral, got {:?}", other),
        }
    }

    #[test]
    fn multi_tool_step_produces_one_alternative_per_tool() {
        let step = Step::new(
            "s",
            "x",
            vec![
                Permit::new(Tool::FileRead).roots(["/etc/"]),
                Permit::new(Tool::Search).roots(["/etc/"]),
            ],
        );
        let g = compile_step(&step).unwrap();
        assert!(g.gbnf.starts_with("# mpodol GBNF"));
        assert!(g.gbnf.contains("root ::= act-0 | act-1"));
        assert_eq!(spellable_tools(&g), vec![Tool::FileRead, Tool::Search]);
    }

    #[test]
    fn empty_step_has_no_grammar() {
        let step = Step::new("s", "x", vec![]);
        assert!(matches!(compile_step(&step), Err(GrammarError::NoPermits { .. })));
    }
}
