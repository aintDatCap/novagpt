# Nova Chat loopback API (experimental v1)

This is an **optional**, owner-operated HTTP adapter to NovaGPT's existing
`ChatGptBrowser` and owner-private `SessionStore`. It is **not** a new browser
engine, a job scheduler, or an MCP tool. NovaGPT continues to work without
Nova Chat, Windmill, Forgejo, or any infrastructure repository.

## Enable and secure it

Configure an environment variable containing a JSON map of gateway principal
IDs to **distinct, randomly generated API bearer credentials**, for example:

```sh
export NOVAGPT_CHAT_API_TOKENS_JSON='{"gateway":"REPLACE_WITH_A_RANDOM_SECRET_AT_LEAST_32_CHARACTERS"}'
novagpt --root /path/to/approved/repo --chatgpt-chrome /absolute/path/to/chrome http --port 8765 --chat-api-port 8766 --no-ngrok
```

The placeholders are **not** credentials. Never paste the real secret into a
repository, command arguments, URLs, diagnostics or logs. The JSON map supports
1–16 principals; principal IDs consist of ASCII letters, digits, underscore or
hyphen (1–32 bytes). Tokens must be distinct printable ASCII strings of 32–256
bytes and must differ from the full-power MCP HTTP token.

The command requires the existing browser feature to be enabled in local settings or an explicit valid `--chatgpt-chrome` path as above; Chrome is discovered and owned by NovaGPT, not this adapter. On a desktop-less Linux host, run the existing command under the separate `script/novagpt_xvfb.py` owner-only wrapper described in [LINUX_VIRTUAL_DISPLAY.md](LINUX_VIRTUAL_DISPLAY.md).

The chat listener binds **only to 127.0.0.1**, independently of the MCP port.
Enabling chat requires the existing optional browser. It cannot be enabled
together with ngrok within the same `http` launch. Never forward the chat
listener directly to the public internet. The external gateway must own
authentication to its users, TLS, rate limiting, CSRF protections, and policy
mapping from portal users to the narrow API principals. NovaGPT API credentials
do not authorize Git, shell, VM or MCP requests; the ordinary MCP secret must
never be given to the portal.

The server and TUI use the same `ChatGptBrowser` implementation; when the
server is composed with one browser handle, the adapter uses that clone and
independent `nova-chat-...` tabs, leaving `tui-chat` and subagent tab IDs
untouched. The standalone TUI does not automatically start a chat HTTP
listener. Starting an independent process against an already-owned Chrome
profile is not supported.

## Stable HTTP contract

All routes are rooted at `/v1/chat` and require
`Authorization: Bearer <NOVA_CHAT_API_TOKEN>`. Only the bearer header is
accepted: cookies and query-string tokens are unsupported. Bodies and status
responses are bounded. The API never returns cookies, browser targets,
profiles, raw errors or Chrome DevTools addresses.

| Route | Method | Response | Purpose |
| --- | --- | --- | --- |
| `/v1/chat/sessions` | POST | 201 | Create a new, unsent session |
| `/v1/chat/sessions` | GET | 200 | List only this principal's sessions |
| `/v1/chat/sessions/{id}` | GET | 200 | Read status, latest available reply, verified conversation URL |
| `/v1/chat/sessions/{id}/send` | POST | 202 or 200 | Submit one operation, or return its existing state |
| `/v1/chat/sessions/{id}/cancel` | POST | 200 | Stop observation and mark possibly sent operation ambiguous |

A send has a required JSON body:

```json
{"operation_id":"gateway-generated-idempotency-key","prompt":"Hello"}
```

An `operation_id` is 1–64 ASCII letters, digits, underscores or hyphens.
The prompt is 1–4096 bytes (non-whitespace). Use a unique operation ID for
each **logical turn**, including when retrying after an HTTP disconnect.
Reusing the **latest** operation ID returns that operation's existing state
without sending the prompt again; reusing an older operation ID returns 409.
At most 32 operations are admitted per session and at most 32 sessions can
exist in the local journal. Body limit: 8192 bytes.

A session object has the following fields:

```json
{
  "id": "nova-chat-opaque-id",
  "status": "ready",
  "operation_id": null,
  "conversation_url": null,
  "reply": null,
  "updated_ms": 1795000000000
}
```

Status is one of `ready`, `running`, `succeeded`, `ambiguous`.
On success, `reply` contains the latest observed browser reply (up to
32 KiB, shortened at a UTF-8 boundary); it is **memory-only** and becomes
`null` after restart. A successfully observed conversation URL is verified
as a ChatGPT conversation URL, saved in the API journal, and best-effort
indexed in the existing owner-private `SessionStore`. On a later request
after restart, a session with a verified URL opens at that URL; otherwise,
an already-succeeded chat with no reopenable URL refuses another turn
rather than silently starting a different conversation.

A 401 means missing or invalid bearer authentication; 404 on session
lookup means it does not exist **for that principal** (including another
principal's ID); 409 means the session is busy, ambiguous, an operation ID
was reused out of order, or safe recovery is unavailable; 429 means a
resource cap was reached. Unexpected storage failures return 503. Browser
failures are not reflected verbatim to clients.

## Durable send ambiguity and cancellation

Before a browser send is started, the API atomically writes `running` and
the operation ID to a private JSON journal. A restart converts any leftover
`running` marker into `ambiguous`; it never re-sends automatically. A
browser error after admission also becomes `ambiguous`, including errors
that might have occurred before the send. Cancelling stops the worker's
observation task, **not** the browser's remotely submitted message, and
therefore also produces `ambiguous`. The client must check the visible
ChatGPT conversation manually and create a new, independently confirmed
operation if appropriate. Do not assume a 409 or an absent HTTP response
proves the message was not delivered.

State is kept owner-private in `chat-api-sessions.json` next to NovaGPT's
existing chat session index. The service takes an exclusive adjacent lock;
it refuses to start if another API instance owns the journal. On Unix,
created journal and lock files are mode 0600 and the parent directory must
be private. At rest, the journal contains no prompts, replies, bearer tokens,
or provider credentials, only metadata and verified conversation URLs.
Removing the journal loses operation reconciliation data and is **not**
a supported recovery procedure.

## Validation limits

Local negative authorization/session isolation and ambiguous-restart tests
require no account credentials. **No live ChatGPT account success is claimed**:
browser focus, manual verification, long responses, account throttling and
Chrome compatibility need controlled live acceptance testing. The HTTP
adapter does not implement streaming tokens, history retrieval beyond the
latest in-memory observed reply, session deletion, multi-host browser
ownership, or user-facing portal authentication. One API principal is a
gateway-controlled scope, not proof of a ChatGPT user identity.
