+++
id = "subagent-focus-gate"
title = "Visible browser focus is a precondition for subagent input"
category = "pitfall"
status = "candidate"
module = "novagpt-browser"
scope = ["crates/novagpt-browser/src/lib.rs"]
sources = ["crates/novagpt-browser/src/lib.rs#L117-L119"]
created_at = "2026-11-18"
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
