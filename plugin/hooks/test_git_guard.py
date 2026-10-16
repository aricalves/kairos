#!/usr/bin/env python3
"""Unit tests for the git guard (KAIROS-T-0497, COLLIERY-I-0631): the
PreToolUse hook on Bash that asks before a push, a merge, a rebase, a tag, a
PR merge, a release and every history rewrite. Stdlib only; run with
`python3 -m unittest plugin/hooks/test_git_guard.py` (or `angreal test
unit`, which includes it).

The tests run `git_guard.py` as a subprocess, the way Claude Code runs it.
No test calls git or a server."""

import importlib.util
import json
import os
import subprocess
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SCRIPT = os.path.join(HERE, "git_guard.py")
SPEC = importlib.util.spec_from_file_location("git_guard", SCRIPT)
git_guard = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(git_guard)

ASKS = (
    "git push",
    "git push origin main",
    "git push -u origin feature",
    "git push --force",
    "git push --force-with-lease origin main",
    "git push origin --delete old-branch",
    "git push --delete origin old-branch",
    "git -C /work/repo push origin main",
    "git -c user.name=x push",
    "git --git-dir=/work/repo/.git push",
    "git merge feature",
    "git merge --no-ff origin/main",
    "git rebase main",
    "git rebase -i HEAD~3",
    "git tag v1.2.0",
    "git tag -a v1.2.0 -m 'release'",
    "git tag -d v1.2.0",
    "git reset --hard HEAD~1",
    "git reset --hard origin/main",
    "git commit --amend",
    "git commit --amend --no-edit",
    "git checkout --force main",
    "git branch -D origin/feature",
    "git branch -rD origin/feature",
    "git branch -r -D origin/feature",
    "gh pr merge 42",
    "gh pr merge --squash",
    "gh release create v1.2.0",
    "gh release edit v1.2.0 --draft=false",
    "gh release delete v1.2.0 -y",
    "git fetch origin && git push origin main",
    "git status; git merge feature",
    "git log | head; git rebase main",
    "git status || git push",
    "(git push origin main)",
    "echo $(git push)",
    "echo `git tag v1`",
    "git add -A\ngit commit -m x\ngit push",
    "cd /work/repo && git push",
    "env FOO=1 git push",
    "FOO=1 git push",
    "nohup git push > log 2>&1",
    # KAIROS-T-0548: a command inside a shell string or an eval.
    'bash -c "git push origin main"',
    "sh -c 'git merge feature'",
    'zsh -lc "git rebase main"',
    "bash -ec 'git fetch && git push'",
    "eval git push",
    'eval "git tag v1"',
    "exec bash -c 'git push'",
    # KAIROS-T-0548: a pull, a clean, a discard, a merge through the API.
    "git pull",
    "git pull --rebase origin main",
    "git pull --ff-only origin main",
    "git clean -fdx",
    "git clean -f",
    "git clean --force -d",
    "git checkout -- .",
    "git checkout -- src/main.rs",
    "git restore .",
    "git restore src/main.rs",
    "gh api repos/o/r/pulls/42/merge -X PUT",
    "gh api -X PUT /repos/o/r/pulls/42/merge/",
)

SILENT = (
    "git status",
    "git log --oneline -5",
    "git fetch origin main",
    "git fetch --all",
    "git diff origin/main",
    "git add -A && git commit -m 'KAIROS-T-0497 git guard'",
    "git checkout -b feature",
    "git branch feature",
    "git branch -d feature",
    "git branch -D feature",
    "git tag",
    "git tag -l",
    "git tag --list 'v*'",
    "git reset HEAD~1",
    "git reset --soft HEAD~1",
    "git worktree add /work/wt -b topic origin/main",
    "grep -rn 'push' plugin/hooks",
    "grep 'git push' README.md",
    "echo 'git push'",
    "cat docs/how-to-push.md",
    "gh pr create --draft --title x",
    "gh pr checks 42 --watch",
    "gh release list",
    "gh release view v1.2.0",
    "ls",
    "",
    # KAIROS-T-0548: the silent neighbours of the new rules.
    "git clean -n",
    "git clean --dry-run -d",
    "git restore --staged src/main.rs",
    "git restore -S .",
    "git checkout feature",
    "git checkout -b topic origin/main",
    "bash -c 'git status'",
    "sh -c ls",
    "eval echo hi",
    "gh api repos/o/r/pulls/42",
    "gh api repos/o/r/pulls/42/comments",
)


def payload(command, tool_name="Bash"):
    return json.dumps(
        {"session_id": "session-0001", "tool_name": tool_name, "tool_input": {"command": command}}
    )


class GitGuardPure(unittest.TestCase):
    def test_each_listed_shape_is_named(self):
        for command in ASKS:
            with self.subTest(command=command):
                self.assertIsNotNone(git_guard.guarded(command))

    def test_each_non_match_is_silent(self):
        for command in SILENT:
            with self.subTest(command=command):
                self.assertIsNone(git_guard.guarded(command))

    def test_the_reason_names_the_command(self):
        self.assertIn("git push", git_guard.guarded("git push origin main"))
        self.assertIn("git rebase", git_guard.guarded("git fetch && git rebase main"))
        self.assertIn("gh pr merge", git_guard.guarded("gh pr merge 42"))
        self.assertIn("--amend", git_guard.guarded("git commit --amend"))
        self.assertIn("--hard", git_guard.guarded("git reset --hard"))
        self.assertIn("--force", git_guard.guarded("git checkout --force main"))

    def test_a_command_that_shlex_cannot_split_is_scanned_by_words(self):
        self.assertIsNotNone(git_guard.guarded("git push 'unterminated"))
        self.assertIsNone(git_guard.guarded("echo 'unterminated"))


class GitGuardHook(unittest.TestCase):
    def run_hook(self, stdin):
        return subprocess.run(
            [sys.executable, SCRIPT],
            input=stdin,
            capture_output=True,
            text=True,
            check=False,
        )

    def test_a_push_gets_an_ask_with_a_one_line_reason(self):
        result = self.run_hook(payload("git push origin main"))
        self.assertEqual(result.returncode, 0, result.stderr)
        output = json.loads(result.stdout)["hookSpecificOutput"]
        self.assertEqual(output["hookEventName"], "PreToolUse")
        self.assertEqual(output["permissionDecision"], "ask")
        self.assertIn("git push", output["permissionDecisionReason"])
        self.assertNotIn("\n", output["permissionDecisionReason"])

    def test_a_non_match_gives_no_output(self):
        result = self.run_hook(payload("git status"))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "")

    def test_another_tool_gives_no_output(self):
        result = self.run_hook(payload("git push", tool_name="Read"))
        self.assertEqual(result.stdout, "")

    def test_bad_input_gives_no_output_and_exit_0(self):
        for stdin in ("", "not json", "[]", json.dumps({"tool_name": "Bash"})):
            with self.subTest(stdin=stdin):
                result = self.run_hook(stdin)
                self.assertEqual(result.returncode, 0)
                self.assertEqual(result.stdout, "")

    def test_the_hook_is_registered_on_bash(self):
        with open(os.path.join(HERE, "hooks.json"), encoding="utf-8") as handle:
            hooks = json.load(handle)
        entries = [
            hook
            for entry in hooks["hooks"]["PreToolUse"]
            if entry.get("matcher") == "Bash"
            for hook in entry["hooks"]
        ]
        self.assertTrue(any("git_guard.py" in hook["command"] for hook in entries))


if __name__ == "__main__":
    unittest.main()
