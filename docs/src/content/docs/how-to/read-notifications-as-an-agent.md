---
title: "Read notifications as an agent"
---

This page describes Kairos 0.12.0.

An agent that works with your agent key gets your notifications. For
example, the agent learns that a person gave it the claim of a task. Kairos
does not start a session. The agent reads the notifications when it calls a
tool.

**Before you start:**

- The agent connects over MCP with your agent key. See
  [Give an agent your key](../give-an-agent-your-key/).
- A service account gets no notifications. Use an agent key.

## 1. Read the block at the end of each answer

While you have unread notifications, each answer of a Kairos tool ends with
a block. The block has the 5 newest unread notifications:

```text
## Your notifications (7 unread)
- 6f1a1f9e-… · 2026-10-12 09:15Z · hand_off · ACME-T-0012: Robin gave you the claim on ACME-T-0012: Fix the login
- …
2 more: call get_events
Mark them read with mark_events_read when you have acted on them.
```

Each line has the id, the time, the kind, the item and the text. The block
stays on each answer until the agent marks the notifications read. A
refusal has no block.

## 2. List all of them

Call the tool `get_events`. It has no arguments. It gives each unread
notification first, newest first, then the 20 newest read notifications.

With REST, use `GET /api/me/notifications`.

## 3. Act, then mark them read

Act on each notification that changes the work. For example, start the task
of a cleared blocker, or put a P0 bug first. Then mark the notifications
read with `mark_events_read`:

```json
{ "ids": ["6f1a1f9e-…", "2b7c…"] }
```

Or mark each unread notification read:

```json
{ "all": true }
```

The answer is one line:

```text
Marked 2 notification(s) read. You have 5 unread.
```

Kairos refuses an id that is not one of your notifications, and changes
nothing.

With REST, use `POST /api/me/notifications/read` with the same body.

Do not mark a notification read before the agent acts on it. You and the
agent share the notifications. When the agent marks a notification read, it
is read in the bell of the web app too.

## The skills of the plugin

The `implement` and `triage` skills of the Kairos plugin read the block.
They call `get_events`, and mark the notifications read after they act.

## Related reading

- [Read your notifications](../read-your-notifications/): the bell and the
  settings.
- [MCP tools: Your notifications](../../reference/mcp-tools/#your-notifications):
  `get_events` and `mark_events_read`.
- [Connect over MCP](../connect-over-mcp/).
