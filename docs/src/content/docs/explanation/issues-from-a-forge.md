---
title: "Issues from a forge"
---

A forge webhook used to record links only. It never made an item, and it
never moved a card (COLLIERY-I-0009). KAIROS-A-0028 changes the first half
of that rule, for issues only: an opened GitHub or GitLab issue that the
forge filter of the repository admits becomes one support task. Webhooks
still never move cards.

This page tells why, and what Kairos does not do with the issue after that.

## Why issues become tasks

A public repository gets issues from people who do not use Kairos. Those
issues need triage, and the team triages its work in Kairos. Before
KAIROS-A-0028, a person read the issues on the forge and made the tasks by
hand, or the issues were not triaged at all.

So the issue comes in as a request to the owner team of the repository: a
task in the entry column of its delivery board, in the Support lane. That is
the place where a request from another team arrives. The board triages it in
its usual way. A person decides, or an agent decides when the board lets
agents decide (KAIROS-T-0428). Kairos adds no second triage flow for issues.

## Who makes the task

The forge system actor of the organization, **Forge**, makes the task
(KAIROS-T-0481). It is not the person who connected the repository: that
person did not ask for the work, and the history of the task must not say
that they did. The login of the author of the issue is on the task and on
its link. Forge is not a login. It has no password and no API key, and the
server refuses it a login of each kind.

Forge is a member of the organization, so it has the right of each member: it
can send a request to any delivery board. It has no other right.

## One task for one issue

Forges send an event again when they think that it did not arrive, and the
import of the open issues (KAIROS-T-0483) reads issues that a webhook took
already. So the key of the intake is the forge's own id of the issue. The
first event or import takes the issue and makes the task. Each later one
finds the key and makes nothing.

## What is not synced

After intake, Kairos does not read the issue again, and it writes nothing to
it:

- A close, a reopen or an edit on the forge does not change the task. An
  issue closed on the forge stays open in Kairos until a person closes the
  task.
- A comment or a label on the forge does not come to Kairos.
- Kairos writes no comment and no label to the issue.

This is on purpose. Two-way sync of two trackers needs a rule for each
conflict, and the forge and Kairos have different states and different
people. The task in Kairos is a copy of the request at the time it arrived.
The link on the task goes back to the issue for what came after.

## The forge filter is the control for noise

A public repository can fill a board with noise. The include and exclude
patterns of `issue_title` and `issue_author` on the repository decide which
issues Kairos takes (KAIROS-T-0479). The usual first rule excludes bots, for
example `^dependabot\[bot\]$`. The same filter decides which branches and
pull requests Kairos records.

## Related

- [Connect a git forge](../../how-to/connect-a-git-forge/)
- [Repositories as execution scope](../repositories-as-execution-scope/)
- [Teams and boards](../teams-and-boards/)
