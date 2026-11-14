# ADR 0005 — Automatic ChatGPT project for subagent conversations

**Date:** 2026-11-12
**Status:** Accepted. Deterministic browser fixture tests pass; live ChatGPT creation has not been verified.

## Context

The project URL option introduced by [ADR 0004](0004-browser-mediated-tui-chat.md) groups new subagent chats, but requires a user to create a ChatGPT project and copy its URL into NovaGPT settings. CodexPro previously automated that setup through the visible ChatGPT interface. NovaGPT now needs the same user-visible outcome while remaining an independent Rust implementation with its own safety contract.

## Decision

- Unless the user supplies a valid project URL override, the first new subagent starts by searching the authenticated ChatGPT sidebar for an exact-name project: **NovaGPT - subagents**. If none exists, the browser uses the visible **New project** dialog to create it.
- The dedicated, user-owned Chrome/ChromeDriver session performs all operations through visible UI controls. No private ChatGPT API requests, cookie extraction, session-token storage or automated challenge handling are introduced.
- Validate the resulting `https://chatgpt.com/g/g-p-<id>/project` URL and the visible exact-name project entry or heading before sending the first worker task. Reuse that worker's tab for discovery and task submission; do not open a separate setup tab. Persist only the validated URL in `novagpt-subagent-project.json` under the existing dedicated Chrome profile. Revalidate the URL at first use after a restart, and rediscover or recreate the project if the cache is stale.
- Serialize project setup with a separate async lock so concurrent workers do not duplicate creation. Reuse the waiting worker's own tab on manual-login/challenge resume; close it if non-manual project setup fails.
- If project discovery, creation or verification fails, reject worker startup before submitting a task. A first-send URL guard still rejects a worker that has navigated away. Reopened conversations and the interactive TUI Chat page are deliberately excluded from this new-chat rule.
- An explicitly configured project URL is authoritative and bypasses automatic setup. No settings migration is needed; users may still use their own project. NovaGPT never deletes or moves existing ChatGPT conversations.

## Consequences and risks

The project is automatically created only when ChatGPT's current UI exposes the expected controls. The UI may change or use other languages; this must fail closed rather than quietly create new chats in the general history. Manual authentication and verification must remain human-driven. Cached URLs are not credentials and do not bypass the first-prompt project check. The first worker after a server restart opens the persisted project URL directly in its own tab, verifies it there and returns to discovery in that same tab if the cache is stale. Removing the project through ChatGPT remains the user's responsibility.

Tests cover concurrent single creation, reuse of an existing project, missing controls, challenge recovery, URL validation and cache rejection. Live project creation and Chrome behavior against a real ChatGPT account require separate verification.

This decision **supersedes only** ADR 0004's manual-only project-creation choice and its rejected alternative of UI-based project creation. All other ADR 0004 decisions remain in effect.
