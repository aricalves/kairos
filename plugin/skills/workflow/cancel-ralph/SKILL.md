---
name: cancel-ralph
description: Stop the Ralph loop of this session, or of this folder after /clear. Then the Stop hook lets the session stop.
argument-hint: "[--loop <session id>]"
disable-model-invocation: true
---

# Cancel the Ralph loop

The arguments are: `$ARGUMENTS`

Run this command. Give the arguments as they are:

```bash
"${CLAUDE_PLUGIN_ROOT}/plugin/hooks/ralph.py" cancel --session "${CLAUDE_SESSION_ID}" $ARGUMENTS
```

The command removes the state file of the loop (`~/.claude/kairos-ralph/<session id>.json`). Then the `Stop` hook lets the session stop.

After `/clear`, the session has a new ID. The old loop then has no session. In this case, the command looks for a loop in this folder:

- One loop in this folder: the command cancels it and names its session.
- Two or more loops in this folder: the command cancels none and names each session. Ask the user which loop to stop. Then run the command again with `--loop <session id>`.

Give the output of the command to the user. The output names the tasks of the loop and the iteration. If no loop is active, the output says so.

The cancel does not change the items. A task keeps its column and its notes. If a task is in Active and its work is not complete, tell the user its short code. Then do the "Abort" section of `/kairos:implement` for that task: the note, the state, the claim and the worktree.

The command also removes state files that are older than 30 days.
