# ADR 0004 — Browser-mediated TUI chat, session index and subagent project

**Date:** 2026-11-09
**Status:** Accepted (manual-only project setup superseded by [ADR 0005](0005-automatic-subagent-project.md)). Deterministic tests implemented; live ChatGPT login, Cloudflare and reply completion remain unverified.

## Context

NovaGPT's product contract treats ChatGPT as the MCP client and NovaGPT as the
server. Users nevertheless asked for three things that are about the opposite
direction: ask ChatGPT directly from the terminal, reopen conversations that
NovaGPT opened, and keep subagent conversations together so they can be deleted
as one group.

The proposal of talking to `chatgpt.com` over HTTP with an account's session
tokens was evaluated and rejected:

- It requires reverse-engineering private web endpoints and a JavaScript
  proof-of-work challenge, plus Cloudflare-resistant TLS fingerprinting.
- OpenAI's consumer terms prohibit bypassing protective measures and
  automatically extracting Output; shared account credentials are also
  prohibited.
- Session tokens are account-takeover material, and "any account" implies
  handling third-party secrets.
- The endpoints and challenge change without notice, so the integration would
  break continuously.

The existing `ChatGptBrowser` already composes a prompt in the user's visible,
authenticated Chrome and returns the assistant text plus the conversation URL.
That path keeps all provider authentication inside the real browser, adds no
credential handling and does not defeat any challenge.

## Decision

1. Add a fourth TUI page, **Chat**, that uses the same owned `ChatGptBrowser`
   and the page id `tui-chat`. It is the server's browser instance when MCP is
   running, otherwise a lazily created one on the same dedicated profile.
   Chat tabs are opened with `open_home()`, deliberately outside the subagent
   project, and skip the project guard.
2. Persist a bounded, owner-private **session index**
   (`chat-sessions.json`, 0600, atomic replace) containing only opaque ids,
   kinds, validated `https://chatgpt.com/c/<id>` URLs and timestamps. Prompts,
   replies, cookies and tokens are never stored. Prompt and reply text stay in
   memory for the current TUI run only; ChatGPT remains the owner of history.
3. Record subagent conversations through a narrow `AgentManager` observer that
   receives only `(agent id, conversation URL)` and forwards the value to a
   blocking writer.
4. Allow an optional **ChatGPT project URL** for subagent tabs. New worker tabs
   start on that project page, and the adapter refuses to submit if ChatGPT
   left the project before the first prompt. The project must be created by the
   user in ChatGPT; NovaGPT never automates project creation.
5. Reopening a session opens a validated conversation URL: TUI chat sessions
   continue in the chat page (`open_at`), subagent sessions open the existing
   browsing tab. A reopened conversation skips the project guard because it
   already exists in the account.

## Invariants

- No HTTP request to `chatgpt.com` outside the browser; no cookie or token is
  read, stored or logged.
- A session URL is validated by shape and origin before any navigation; a
  tampered index entry is ignored rather than opened.
- No prompt replay: reopening navigates to a conversation; it never resubmits
  earlier text.
- The chat tab is never a subagent tab and never evicted by agent tab limits.
- The dedicated Chrome profile is launched by one owner; the server reuses a
  chat-owned browser instead of launching a second Chrome on the same profile.
- The index is auxiliary: a missing, damaged or oversized file degrades to an
  empty list without blocking the TUI or MCP.

## Consequences and limits

- Live ChatGPT login, Cloudflare verification and reply completion remain
  unverified in this repository; Windows Chrome execution is still untested.
- The project page must exist and remain accessible; deleting it deletes its
  conversations, which is the intended cleanup path.
- The TUI transcript is not restored after a restart; reopening restores the
  ability to continue the conversation, not the local view of old turns.
- If the TUI chat owns Chrome when MCP starts, the server reuses that browser;
  starting a second mode that expects to own the profile can still fail closed
  with `ProfileBusy`.

## Alternatives rejected

- Unofficial HTTP client for `chatgpt.com` (ToS, security, fragility).
- Official OpenAI API provider (a different billing model and not requested;
  it would need its own ADR and backend).
- Automating project creation through ChatGPT's sidebar and dialogs (fragile
  localized UI; the project URL is a one-time user action).
- Persisting transcripts to disk (new private data surface with no functional
  need; ChatGPT already owns the history).

## Related

- [0002 — Optional VM and ChatGPT Browser subagents](0002-optional-vm-and-browser-agents.md)
- [0003 — Rust-native persistent browser driver](0003-native-browser-driver.md)
- [ARCHITECTURE.md](../ARCHITECTURE.md), [TUI_SETUP.md](../TUI_SETUP.md),
  [CAPABILITIES.md](../CAPABILITIES.md)
