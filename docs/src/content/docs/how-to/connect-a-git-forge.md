---
title: "Connect a git forge"
---

Connect a GitHub or GitLab repository. Then Kairos does these things:

- It records each branch and pull request that the forge filter of the
  repository admits. The repository and the **In flight** panel of the owner
  team show them, with or without a short code.
- It links a branch or a pull request that names a short code to that work
  item.
- It makes one support task from each opened issue that the filter admits
  (KAIROS-A-0028).

Kairos can also take the open issues on an interval, with no webhook. See
[section 6](#6-take-issues-on-an-interval-with-no-webhook). Use it when
the forge cannot reach Kairos, for example on a private network.

**Before you start (for the webhook):**

- **Kairos is reachable from the forge.** Webhooks are inbound. For a deployed
  ingress that is ordinary; for local development you need a tunnel
  (`cloudflared`, `ngrok`), because `localhost` is not routable from
  github.com.
- The repository is **registered** in Kairos, with a `github` or `gitlab` forge.
  If it is not:

  ```sh
  kairos repos create --forge github --name acme/payments-api \
    --repo-url https://github.com/acme/payments-api --team platform \
    --slug payments-api
  ```
- You are an **organization admin**. Connecting and rotating are admin-only.
- Two configuration values are set:

  ```sh
  KAIROS_PUBLIC_URL=https://kairos.acme.example      # externally reachable base URL
  KAIROS_WEBHOOK_SIGNING_KEY=<a long random secret>  # every webhook secret derives from this
  ```

  Without them the connection endpoints answer `501` and the integration is
  simply off, not degraded. Neither has a dedicated Helm value; set them through
  the chart's `extraEnv`. Treat the signing key like any other deployment
  secret, and do not rotate it casually: **changing it invalidates every
  existing connection**, because per-connection secrets are derived from it
  rather than stored.

## 1. Connect the repository

```sh
curl -X POST https://<host>/api/forge-connections \
  -H "Authorization: Bearer <your-admin-token>" \
  -H 'Content-Type: application/json' \
  -d '{"repository":"payments-api"}'
```

The response carries the `webhook_url` and the `webhook_secret`. **The secret is
shown exactly once.** Paste it into the forge in step 2 before you lose it; if
you do, rotate (below) rather than reconnecting.

*Admin → Repositories → Connect webhook* in the GUI does the same thing and
shows the secret under the same one-time rule.

The repository's forge determines the webhook dialect. A repository registered
with forge `other` can own tasks but cannot be connected.

## 2. Add the webhook in the forge

**GitHub** — *Settings → Webhooks → Add webhook*:

| Field | Value |
|---|---|
| Payload URL | the returned `webhook_url` |
| Content type | `application/json` |
| Secret | the returned `webhook_secret` |
| Events | **Pull requests**, **Branch or tag creation**, **Branch or tag deletion** and **Issues** |

**GitLab** — *Settings → Webhooks → Add new webhook*:

| Field | Value |
|---|---|
| URL | the returned `webhook_url` |
| Secret token | the returned `webhook_secret` |
| Triggers | **Merge request events**, **Push events** and **Issues events** |

Do not select the issues events if you do not want tasks from issues. Kairos
reads no other event, and it answers each other event with `200`.

## 3. Set the forge filters

The forge filters decide which branches, pull requests and issues Kairos
records. An organization admin sets them on each repository. Each field has
one include pattern and one exclude pattern:

| Field | Applies to |
|---|---|
| `branch_name` | a branch: its name |
| `pr_head_branch` | a pull request: its head branch |
| `pr_title` | a pull request: its title |
| `pr_author` | a pull request: the login of its author |
| `issue_title` | an issue: its title |
| `issue_author` | an issue: the login of its author |

Kairos admits an event when each field matches its include pattern. The
field must also not match its exclude pattern. The include pattern is `.*`
by default. The exclude pattern is empty by default, and an empty exclude
pattern excludes nothing.

A pattern is a Rust regular expression of 512 bytes or fewer. It can match
any part of the value. Use `^` and `$` to match all of the value. Kairos
refuses a pattern that is not correct or is too long. The error names the
field (for example `forge_filters.pr_author.exclude`).

Example: do not record the pull requests and the issues of Dependabot.

```sh
kairos repos update payments-api \
  --exclude 'pr_author=^dependabot\[bot\]$' \
  --exclude 'issue_author=^dependabot\[bot\]$'
```

Example: record only the branches that start with `feature/` or `fix/`.

```sh
kairos repos update payments-api --include 'branch_name=^(feature|fix)/'
```

You can also set the filters on the page *Admin → Repositories*: push Edit,
then *Update forge filters*. The MCP tool `update_repository` sets them with
the argument `forge_filters`. `kairos repos show payments-api` shows the
filters.

## 4. Use it

Name the short code in the branch — the convention worth adopting team-wide —
or in the PR title or description. All three of these link:

```text
dylan/PLATFORM-T-0001-fix-auth
Fix login (PLATFORM-T-0001)
…a description mentioning PLATFORM-T-0001
```

The link appears when the branch is pushed or the PR is opened, and merging
updates it live. Activity on a repository rolls up to its owning team's In
flight panel even for work items that carry no team.

Kairos also records a branch or a pull request with no short code. The In
flight panel of the team and the repository show it under **Not linked to an
item**. Kairos closes a deleted branch and removes it from the list.

## 5. Take issues

When the webhook has the issues events, each opened issue that the filter
admits becomes one task:

- the task type and the lane are `support`;
- the task is in the entry column of the delivery board of the owner team of
  the repository;
- the system actor **Forge** creates it;
- the task links to the issue: its URL, its number and the login of its
  author.

The board triages the task as it triages each request. A person decides, or
an agent decides when the board lets agents decide. Kairos makes one task for one issue. A
second delivery of the same event makes no second task.

After that, Kairos does not read the issue again. A close, a reopen, an
edit, a comment or a label on the forge does not change the task. Kairos does
not take a GitLab confidential issue.

With a write token on the repository, the issue follows the task. A done task
closes the issue, and a reopened task opens it again. See
[Turn on forge write-back](../turn-on-github-write-back/#write-back-to-issues).

### Import the open issues

Kairos takes only the issues that people open after you add the issues
events. To take the older open issues, import them one time:

```sh
kairos repos import-issues payments-api --wait
```

Or push *Import open issues* on the page *Admin → Repositories*. An
organization admin does this. The import uses the read token of the
repository (see [Read a private repository](../read-a-private-repository/)).
It needs no webhook connection. The token must have these permissions:

- GitHub: **Issues: read** on the repository.
- GitLab: the scope **read_api**.

The import sends each open issue through the same path as an opened issue.
The filter decides, then Kairos makes one task for each issue. The result gives the count of
issues that it imported, that it skipped (taken before) and that the filter
excludes. When you run the import again, it makes no new task for an issue
that it took.

The import is a job of the server, so a large repository does not stop the
request. The command and the button start the job and come back at once.
One import of a repository runs at a time: when you start an import while
one waits or runs, you get that import. To see the state and the counts:

```sh
kairos repos import-status payments-api
```

The page *Admin → Repositories* shows the last import under the repository.
When the forge gives its rate limit, the import waits until the time that
the forge gives, and then continues. The state is then `waiting`. When the
forge refuses the token, the import fails, and the error names the
permission.

## 6. Take issues on an interval, with no webhook

A webhook needs the forge to reach Kairos. A deployment on a private network
cannot receive one. For that case, give the repository an import interval.
Kairos then asks the forge for the open issues on that interval. It needs
no webhook connection. It needs no `KAIROS_PUBLIC_URL` and no
`KAIROS_WEBHOOK_SIGNING_KEY`.

Before you start:

- You registered the repository with a `github` or `gitlab` forge.
- The repository has a read token with the permission for issues. See
  [Read a private repository](../read-a-private-repository/). The permission
  is **Issues: read** on GitHub and **read_api** on GitLab.
- You are an organization admin.

Set the interval with one of these:

```sh
kairos repos update payments-api --import-interval 15m
```

- The page *Admin → Repositories*: choose the interval in *Import open
  issues every*, then push *Apply interval*. A person who is not an
  organization admin sees the interval and cannot change it.
- The MCP tool `update_repository`, with the argument `issue_import_interval`.
- `PATCH /api/repositories/{slug}`, with the field `issue_import_interval`.

The interval is `off`, or a number with a unit: `90s`, `15m`, `1h` or `1d`.
The default is `off`. The shortest interval is `1m`. The longest is `30d`.
Kairos refuses another value, and the error names the field.

### What a run does

Each run is a sync. It does not read "what is new since the last run".

- The run lists every open issue of the repository.
- It compares each issue with the record of the issues that Kairos took. The
  key of the record is the id that the forge gives to the issue.
- An issue with no record becomes one support task. The forge filter
  decides, and the task is the same as the task of section 5.
- An issue that has a record makes nothing.

A missed run leaves no gap. The server can be down, or a rate limit can
stop a run. The next run compares the full list again, so it takes each
issue that is still missing. An issue that someone opens and closes between
two runs makes no task. A closed issue does not change its task.

A webhook and an interval can work on the same repository. They share the
record, so one issue makes one task.

### Read the state

```sh
kairos repos import-status payments-api
```

The command prints the interval and the last complete run. It prints the
next run and the count of issues in the record. It also prints the counts of
the last run. The page *Admin → Repositories* shows the same under the
repository.

A run that stops half way keeps the tasks that it made. It does not count as
complete. The next run goes on from the full list.

If a run fails, the state shows the error. A forge that refuses the read
token gives an error that names the permission. The next run tries again.
When the forge gives its rate limit, the run waits until the time that the
forge gives.

Kairos never runs two imports of one repository at the same time. This holds
when a person starts an import by hand, and when two servers share one
database. The command `kairos repos import-issues` starts the same import.
It needs only the read token.

## Rotate the secret

Rotation changes **both** the secret and the delivery URL, because the secret
derives from the connection id. Update both fields in the forge, not just the
secret:

```sh
curl -X POST https://<host>/api/forge-connections/<id>/rotate \
  -H "Authorization: Bearer <admin>"
```

## Disconnect

```sh
curl -X DELETE https://<host>/api/forge-connections/<id> \
  -H "Authorization: Bearer <admin>"
```

The repository and its tasks stay; only the webhook connection goes. Delete the
webhook in the forge too, or it will keep delivering to a dead endpoint.

## Limits to expect

- **A missing link is not evidence of a broken webhook.** Deliveries naming
  short codes that do not exist here are accepted and ignored. Kairos also
  accepts and ignores an event that the forge filter excludes. Check the forge's
  delivery log and the filters before you go looking at Kairos.
- **PR state never moves cards** — so do not wire a column to a merge. Move
  cards from Kairos. (Why:
  [Flight levels](../../explanation/flight-levels/#why-the-boards-are-configurable).)
- **Kairos writes to a pull request or an issue only with a write token.**
  See [Turn on forge write-back](../turn-on-github-write-back/). Kairos writes
  to an issue only when the issue became a task.
- **An issue closed on the forge stays open in Kairos.** Close the task in
  Kairos.
- **Editing a short code out of a PR removes that link.** Re-add the code to get
  it back.
- **Replays are safe.** A redelivered or out-of-order event cannot regress
  state; a replayed "opened" will not un-merge a merged PR. Re-send deliveries
  freely when debugging.

## Related

- [Forge connections](../../reference/rest/execution-scope/) — the endpoints and
  their responses
- [Events](../../reference/events/)
- [Repositories as execution scope](../../explanation/repositories-as-execution-scope/)
- [Issues from a forge](../../explanation/issues-from-a-forge/)
- [Configuration](../../reference/configuration/#forge-webhooks)
