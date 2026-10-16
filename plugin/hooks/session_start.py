#!/usr/bin/env python3
"""SessionStart hook: inject the repo's Kairos wiring as session context.

Reads `.claude/kairos.local.md` (written by /kairos:bootstrap per
KAIROS-A-0014) and emits its contents — deployment, tenant, repository,
stream, boards — as `additionalContext`, plus an instruction to pull live
state. With a `repository` (KAIROS-A-0019, T-0108) the hint is
repo-scoped: read the repository (its "how to work here" description and
in-flight PRs), then the team board narrowed to that repository — the
session's queue. Without one it stays board-scoped as before. The team
decides the board and the repository is a link (COLLIERY-A-0023), so the
hint also says to name a board at create and how to send a request.

v1 design decision (KAIROS-T-0034): hooks have no OAuth token broker —
tokens live with the MCP client (A-0014), and a shell hook cannot drive the
client's browser OAuth flow (A-0011). So this hook NEVER authenticates and
holds no secrets. It performs one unauthenticated reachability probe
(`GET <deployment_url>/healthz`, short timeout) and then instructs the
agent to call `my_boards` / `board_items` over the already-authenticated
kairos MCP connection for live items. Offline degrades to a note, never a
failure.

Not configured (no kairos.local.md) -> silent exit 0. Python stdlib only.
"""

import json
import os
import shutil
import subprocess
import time
import sys
import urllib.request

FRONTMATTER_KEYS = (
    "deployment_url",
    "tenant",
    "repository",
    "delivery_stream",
    "team_board",
    "initiative_board",
    "adr_board",
)


def read_frontmatter(path):
    """Parse the YAML frontmatter's simple `key: value` pairs (no external
    YAML dependency; the /bootstrap contract writes flat scalar keys)."""
    with open(path, encoding="utf-8") as f:
        lines = f.read().splitlines()
    if not lines or lines[0].strip() != "---":
        return {}
    values = {}
    for line in lines[1:]:
        if line.strip() == "---":
            break
        if ":" not in line or line.lstrip() != line:
            continue
        key, _, raw = line.partition(":")
        value = raw.strip().strip("'\"")
        if key.strip() in FRONTMATTER_KEYS and value:
            values[key.strip()] = value
    return values


class Budget:
    """One time budget for every probe of a session start (KAIROS-T-0525):
    each probe gets what is left of it, never less than `floor`, so three
    slow probes cannot add up past the timeout of the hook."""

    def __init__(self, seconds=3.0, floor=0.2, clock=time.monotonic):
        self.deadline = clock() + seconds
        self.floor = floor
        self.clock = clock

    def left(self):
        return max(self.floor, self.deadline - self.clock())


def probe(url, timeout=3):
    """Unauthenticated reachability check; returns a short status note."""
    try:
        with urllib.request.urlopen(url + "/healthz", timeout=timeout) as response:
            if response.status == 200:
                return "deployment reachable"
            return f"deployment answered /healthz with HTTP {response.status}"
    except Exception:
        return (
            "deployment NOT reachable right now (offline?) — "
            "local wiring below is still valid; live board state unavailable"
        )


def live_state_hint(values):
    """The instruction that points the agent at live state over MCP. With a
    `repository` the session is repo-scoped (KAIROS-A-0019); without one it
    is board-scoped, exactly as before T-0108."""
    repository = values.get("repository")
    # `team_board` is a board SLUG (what `board_items` resolves), per the
    # bootstrap contract.
    team_board = values.get("team_board") or "unset"
    # `adr_board` (COLLIERY-T-3102): the ADR board of the team, or the ADR
    # board of the organization when the team has none.
    adr_board = values.get("adr_board") or "unset"
    if repository:
        return (
            f"This checkout is repository `{repository}`. For live state, call the "
            f"kairos MCP tools now: `get_repository` for `{repository}` (read its "
            "\"How to work here\" description and in-flight PRs first). The result "
            "lists the documents and the ADRs that impact this repository: its "
            "vision, its architecture and the decisions about it. Read them with "
            "`get_item` before you plan work. Then call "
            f"`board_items` for the board of your team ({team_board}) with "
            f"`repository={repository}`. That is your queue. The board without "
            "the filter shows all the work of your team. The work of other teams "
            "in this repository is not in your queue. Find it with `search` and "
            f"the filter `repository={repository}`. When you create a task, name "
            "a board in `board` (the CLI and REST also accept a team). The board "
            "decides the team. The repository is an optional link: "
            f"`repository={repository}` says where the code is and does not "
            "choose the board. Work for a different team is a request: name the "
            "delivery board of that team in `board`. The request goes to the "
            "entry column, in the support lane. An ADR about the work of your "
            "team goes on the ADR board of your team: `create_item` with "
            f"`item_type: adr` and `board={adr_board}`. The "
            "MCP client holds the authenticated session; this hook "
            "intentionally carries no credentials."
        )
    return (
        "For live board state, call the kairos MCP tools now: `my_boards`, "
        f"then `board_items` for the team board ({team_board}) to see "
        "active/todo items and standing buckets. No `repository` is wired: "
        "re-run /kairos:bootstrap to detect it from the git remote and get a "
        "repo-scoped queue. The MCP client holds the authenticated session; "
        "this hook intentionally carries no credentials."
    )


def server_version(url, timeout=3):
    """The version of the server (`GET /version`, open), or None."""
    try:
        with urllib.request.urlopen(url + "/version", timeout=timeout) as response:
            return json.load(response).get("version")
    except Exception:
        return None


def cli_version(timeout=3):
    """The version of the `kairos` CLI on PATH, or None."""
    binary = shutil.which("kairos")
    if not binary:
        return None
    try:
        out = subprocess.run(
            [binary, "--version"], capture_output=True, text=True, timeout=timeout
        ).stdout
    except Exception:
        return None
    parts = out.split()
    return parts[-1] if parts else None


def version_tuple(version):
    parts = []
    for part in version.lstrip("v").replace("-", ".").split(".")[:3]:
        parts.append(int(part) if part.isdigit() else 0)
    return tuple(parts)


def plugin_version():
    """The version of the plugin: `.claude-plugin/plugin.json` at
    `CLAUDE_PLUGIN_ROOT`, else two folders above this hook. None when it
    cannot be read."""
    root = os.environ.get("CLAUDE_PLUGIN_ROOT")
    if not root:
        here = os.path.dirname(os.path.abspath(__file__))
        root = os.path.dirname(os.path.dirname(here))
    try:
        with open(os.path.join(root, ".claude-plugin", "plugin.json"), encoding="utf-8") as f:
            version = json.load(f).get("version")
    except (OSError, ValueError, AttributeError):
        return None
    return version if isinstance(version, str) and version else None


def cli_note(cli, server, plugin=None):
    """One line when the CLI is older than the server (KAIROS-T-0432) or
    than the plugin (KAIROS-T-0513), else None (pure; testable). An old CLI
    reads an old index schema, so the code tools of the plugin are off."""
    if not cli or not any(c.isdigit() for c in cli):
        return None
    behind = []
    if server and version_tuple(cli) < version_tuple(server):
        behind.append(f"the server is {server}")
    if plugin and version_tuple(cli) < version_tuple(plugin):
        behind.append(f"the plugin is {plugin}")
    if not behind:
        return None
    return (
        f"- cli: the kairos CLI is {cli}; {' and '.join(behind)}. "
        "Tell the user. Update the CLI: `kairos self-update` installs the "
        "version of the server."
    )


def build_context(values, status, note=None):
    """The full additionalContext text (pure; testable)."""
    lines = [
        "Kairos wiring for this repo (.claude/kairos.local.md; "
        "re-run /kairos:bootstrap to change):",
    ]
    for key in FRONTMATTER_KEYS:
        lines.append(f"- {key}: {values.get(key) or '(not set)'}")
    lines.append(f"- status: {status}")
    if note:
        lines.append(note)
    if "NOT reachable" not in status and values.get("deployment_url"):
        lines.append(live_state_hint(values))
    return "\n".join(lines)


def rules_at_start(values, session):
    """The Kairos rules for the start of a wired session, and the marker
    that keeps the PreToolUse hook silent after (KAIROS-T-0525). Empty when
    the marker was there already or the session id is not usable."""
    import kairos_rules  # here, not at the top: kairos_rules imports this module

    if not kairos_rules.mark(session):
        return ""
    return kairos_rules.build_context(values.get("deployment_url", ""))


def main():
    try:
        payload = json.loads(sys.stdin.read() or "null")
    except ValueError:
        payload = None
    session = payload.get("session_id") if isinstance(payload, dict) else None
    project_dir = os.environ.get("CLAUDE_PROJECT_DIR") or os.getcwd()
    config_path = os.path.join(project_dir, ".claude", "kairos.local.md")
    if not os.path.isfile(config_path):
        return  # not a Kairos-bootstrapped repo: stay silent

    values = read_frontmatter(config_path)
    deployment = values.get("deployment_url", "")
    budget = Budget()
    status = probe(deployment, budget.left()) if deployment else "no deployment_url configured"
    server = None
    if deployment and "NOT reachable" not in status:
        server = server_version(deployment, budget.left())
    note = cli_note(cli_version(budget.left()), server, plugin_version())

    context = build_context(values, status, note)
    rules = rules_at_start(values, session)
    if rules:
        context = context + "\n\n" + rules
    json.dump(
        {
            "hookSpecificOutput": {
                "hookEventName": "SessionStart",
                "additionalContext": context,
            }
        },
        sys.stdout,
    )


if __name__ == "__main__":
    main()
