# Development rules

NovaGPT is an independent Rust project; keep changes inside this repository and avoid modifying unrelated applications. Keep the workspace small, with explicit errors and no `unwrap` in production paths. Prefer domain types and narrow APIs over broad trait hierarchies. Every stateful resource needs a defined lifetime and cancellation/shutdown behavior.

The multi-chat product contract is one real shared filesystem across many MCP sessions; an MCP transport session is never a ChatGPT conversation identity. Keep workspace targeting explicit and fail closed on unauthorized paths.

Before proposing a release, run formatting, Clippy as errors, unit/integration tests, concurrency and security regressions on supported platforms. Report both passes and failures accurately. Preserve a verified rollback path; never claim production readiness without verified results in docs/CAPABILITIES.md.

## Contributor reference

Read [AGENTS.md](AGENTS.md) for the task workflow and mandatory boundaries. Use [docs/ENGINEERING_STANDARDS.md](docs/ENGINEERING_STANDARDS.md) for module/API/dependency design, [docs/EXTENSION_PLAYBOOK.md](docs/EXTENSION_PLAYBOOK.md) when adding features, and [docs/QUALITY_GATES.md](docs/QUALITY_GATES.md) for required acceptance evidence. Record nontrivial architecture tradeoffs in [docs/decisions/](docs/decisions/README.md).
