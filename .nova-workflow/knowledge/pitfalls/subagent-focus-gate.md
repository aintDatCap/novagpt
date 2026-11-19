+++
schema_version = 1
title = "Visible browser focus is a precondition for subagent input"

[[evidence]]
file = "crates/novagpt-browser/src/lib.rs"
commit = "e734dc9c2125fdd671a4bb47b28c9b0016340773"
source = "legacy:docs/knowledge/pitfalls/subagent-focus-gate.md"
status = "candidate"
+++
# Visible browser focus is a precondition for subagent input

The browser adapter has a distinct `InteractionBlocked` error for cases in
which a ChatGPT tab cannot be made visible and focused. Its error contract says
that no prompt was submitted. Parallel startup of browser-driven workers can
encounter this condition on a desktop session.

Operational implication: inspect the specific worker state and browser focus
before considering a retry. Never replay a prompt after a later-stage
`AmbiguousSubmission` result: that state does not prove no submission occurred.

This note is **candidate** knowledge. The current source establishes the
failure classification, but the frequency and concurrency cause have not been
independently verified with a reproducible fixture.
