---
title: "Connect the Claude Desktop app"
---

Give the Claude Desktop app the Kairos tools. It gets the work profile
(`whoami`, `my_work`, `search`, `get_item` and the rest) from the same `/mcp`
endpoint that agents use.

**Before you start:** you need the URL of the deployment and a way to
authenticate. Which way depends on how the deployment signs people in.

- The deployment has an OIDC issuer: use a custom connector. Desktop signs you
  in through the issuer.
- The deployment uses local accounts, or you want to connect as an agent: use
  a bridge with your agent key. See
  [Give an agent your key](../give-an-agent-your-key/).

## Route 1: a custom connector (OIDC)

1. In the Desktop app, open **Settings → Connectors → Add custom connector**.
2. Enter `https://kairos.example.com/mcp` as the URL.
3. Save, then sign in when Desktop asks. Kairos answers the first request
   with an RFC 9728 challenge, and Desktop follows it to the issuer.

The connector carries your own identity. In the app you can do what you can
do in the browser.

## Route 2: a bridge with an agent key

Desktop cannot send a bearer header of its own. The `mcp-remote` package runs
as a local process, connects to `/mcp`, and adds the header.

1. Find the config file. On macOS it is
   `~/Library/Application Support/Claude/claude_desktop_config.json`. On
   Windows it is `%APPDATA%\Claude\claude_desktop_config.json`.
2. Add a `kairos` entry under `mcpServers`:

```json
{
  "mcpServers": {
    "kairos": {
      "command": "/Users/<you>/.local/node/bin/npx",
      "args": [
        "-y", "mcp-remote@latest",
        "https://kairos.example.com/mcp",
        "--transport", "http-only",
        "--header", "Authorization:Bearer ${KAIROS_MCP_KEY}"
      ],
      "env": {
        "KAIROS_MCP_KEY": "kairos_sk_acme_…"
      }
    }
  }
}
```

3. Restart the Desktop app. The tools icon in the chat box lists `kairos`.
4. Call `whoami` first: it names the person or agent and the boards.

Notes on the entry:

- Write the full path of `npx`. Desktop starts a server with a short `PATH`,
  and a bare `npx` often fails there.
- `--transport http-only` skips the SSE fallback. Kairos serves streamable
  HTTP with no sessions, and `mcp-remote` needs no more than that.
- The key sits in `env`, and the header names it as `${KAIROS_MCP_KEY}`. The
  key does not appear in the command line or in the process list.
- Write no space after the colon in `Authorization:Bearer`. Some versions of
  `mcp-remote` split the header on the first space.
- A multi-tenant deployment with no base domain needs the tenant: add
  `"--header", "X-Tenant:acme"`. See
  [Tenant resolution](../connect-over-mcp/#tenant-resolution).

## What Desktop gets

The work profile: the forty tools that read and write items. For the
administration tools, add `?profile=admin` to the URL. See
[Profiles](../../reference/mcp-tools/#profiles).

The Claude Code plugin also runs two local servers, the code index
(`kairos-code`) and dispatch. They are binaries of the plugin, and the Desktop
app does not get them.

## If it does not connect

- `401`: the key is wrong, or revoked. Make a new one and replace it in `env`.
- The server list shows `kairos` with no tools: `mcp-remote` kept a stale
  state. Remove the folder `~/.mcp-auth` and restart the app.
- The app says the command was not found: the path of `npx` is wrong. Run
  `which npx` in a terminal and copy the answer.

## Related

- [Connect over MCP](../connect-over-mcp/): the handshake and the errors
- [Give an agent your key](../give-an-agent-your-key/)
- [MCP tools](../../reference/mcp-tools/)
