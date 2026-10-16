#!/usr/bin/env python3
"""The git guard (KAIROS-T-0497, COLLIERY-I-0631): a PreToolUse hook on Bash
that asks before a command that pushes, merges, rebases, tags, releases or
rewrites history. It asks every time (Dylan, 2026-10-15); it does not look
for a "go" in the conversation.

The command is split into its simple commands (`&&`, `||`, `;`, `|`, a new
line, a subshell, `$(...)` and backticks), each simple command into words,
and the leading `VAR=value` words and wrappers (`env`, `nohup`, `sudo`,
`time`, `command`) are dropped. Then:

    git push ...                         any push, also --delete
    git merge ...                        into any branch; the user decides
    git rebase ...
    git tag <name> | -a | -d | -f        a list (`git tag`, `-l`) is silent
    git reset --hard ...
    git commit --amend ...
    git branch -D|-d of a remote-tracking branch (`-r`, or `origin/x`)
    git <anything> --force | --force-with-lease | -f on push
    gh pr merge ...
    gh release create | edit | delete ...

A match gives `permissionDecision: "ask"` with a one-line reason that names
the command. Anything else gives no output. Each error gives no output and
exit 0: the hook never blocks a tool call by accident. The hook holds no
credentials and reads no file. Python stdlib only.
"""

import json
import re
import shlex
import sys

SEPARATORS = re.compile(r"&&|\|\||[;|\n()`]|\$\(")
ASSIGNMENT = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*=")
WRAPPERS = ("env", "nohup", "sudo", "time", "command", "exec")
GIT_OPTIONS_WITH_VALUE = ("-C", "-c", "--git-dir", "--work-tree", "--namespace", "--exec-path")
GH_RELEASE_WRITES = ("create", "edit", "delete", "upload", "delete-asset")
FORCE_FLAGS = ("--force", "--force-with-lease", "--force-if-includes")


def simple_commands(command):
    """The simple commands of a shell command line."""
    return [part.strip() for part in SEPARATORS.split(command) if part and part.strip()]


def words_of(simple):
    """The words of one simple command, without the leading assignments and
    wrappers. A quote that shlex cannot read falls back to a split on
    spaces, so a guarded command is still seen."""
    try:
        words = shlex.split(simple, comments=False)
    except ValueError:
        words = simple.split()
    while words and (ASSIGNMENT.match(words[0]) or words[0] in WRAPPERS):
        words = words[1:]
    return words


def git_subcommand(words):
    """(subcommand, its arguments) of a `git` word list, after the global
    options of git; (None, []) when there is none."""
    rest = words[1:]
    while rest:
        word = rest[0]
        if word in GIT_OPTIONS_WITH_VALUE:
            rest = rest[2:]
        elif word.startswith("-"):
            rest = rest[1:]
        else:
            return word, rest[1:]
    return None, []


def has_short_flag(args, letter):
    """True when a `-xyz` cluster in `args` holds `letter`."""
    for arg in args:
        if arg.startswith("-") and not arg.startswith("--") and letter in arg[1:]:
            return True
    return False


def guarded_git(words):
    """The reason to ask for one `git` command, or None."""
    subcommand, args = git_subcommand(words)
    if subcommand is None:
        return None
    if any(arg in FORCE_FLAGS or arg.startswith("--force-with-lease=") for arg in args):
        return f"`git {subcommand} --force`: a history rewrite needs the user's go, each time."
    if subcommand == "push":
        return "`git push`: a push needs the user's go, each time."
    if subcommand == "merge":
        return "`git merge`: a merge needs the user's go, each time."
    if subcommand == "rebase":
        return "`git rebase`: a rebase rewrites history; it needs the user's go, each time."
    if subcommand == "tag":
        listing = not args or any(arg in ("-l", "--list", "-n") or arg.startswith("-n") for arg in args)
        if listing:
            return None
        return "`git tag`: a tag starts a release; it needs the user's go, each time."
    if subcommand == "reset" and "--hard" in args:
        return "`git reset --hard`: a history rewrite needs the user's go, each time."
    if subcommand == "commit" and "--amend" in args:
        return "`git commit --amend`: a history rewrite needs the user's go, each time."
    if subcommand == "branch":
        deleting = has_short_flag(args, "D") or has_short_flag(args, "d") or "--delete" in args
        remote = has_short_flag(args, "r") or "--remotes" in args
        remote = remote or any("/" in arg for arg in args if not arg.startswith("-"))
        if deleting and remote:
            return "`git branch -D` of a remote-tracking branch needs the user's go, each time."
    return None


def guarded_gh(words):
    """The reason to ask for one `gh` command, or None."""
    args = [word for word in words[1:] if not word.startswith("-")]
    if args[:2] == ["pr", "merge"]:
        return "`gh pr merge`: a merge needs the user's go, each time."
    if len(args) >= 2 and args[0] == "release" and args[1] in GH_RELEASE_WRITES:
        return f"`gh release {args[1]}`: a release needs the user's go, each time."
    return None


def guarded(command):
    """The reason to ask for a Bash command, or None when nothing in it is
    guarded (pure; testable)."""
    if not isinstance(command, str):
        return None
    for simple in simple_commands(command):
        words = words_of(simple)
        if not words:
            continue
        reason = None
        if words[0] == "git":
            reason = guarded_git(words)
        elif words[0] == "gh":
            reason = guarded_gh(words)
        if reason:
            return reason
    return None


def main():
    try:
        payload = json.loads(sys.stdin.read() or "null")
        if not isinstance(payload, dict) or payload.get("tool_name") != "Bash":
            return
        tool_input = payload.get("tool_input")
        if not isinstance(tool_input, dict):
            return
        reason = guarded(tool_input.get("command"))
        if not reason:
            return
        json.dump(
            {
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": "ask",
                    "permissionDecisionReason": reason,
                }
            },
            sys.stdout,
        )
    except Exception:  # never block a tool call by accident
        return


if __name__ == "__main__":
    main()
