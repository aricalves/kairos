---
title: "Build the code index again"
---

Kairos keeps a code index of each repository. The builder of the server
updates it after each push to the default branch. It also makes the first
index of a repository that has none. This page tells you how to see what
the builder did, and how to ask for a new index.

## See the runs of the builder

Each run of the builder leaves a record. A run is a build after a push, a
first build, a build on request, or an upload. Each member can read the
records.

```bash
kairos repos builds <slug>
```

The command shows the runs, newest first. Each run has its trigger,
outcome, commit, count of symbols, and the text of a failure. Use
`--limit <N>` for more runs (100 at most).

The same records are in the GUI. Find the repository on the team page, or
on the page Admin, Repositories, and open "Show code index". The panel
shows the newest index, the last runs and the indexed commits. It updates
when a run starts or ends. The REST route is
`GET /api/repositories/{slug}/code-indexes/builds`.

A run can have the text "The server stopped during the run". Then no build
job of the repository had that run any more. The next run of the repository
replaces it. When a server stops during a build, another server takes the
job, and the run continues there.

A new version of Kairos can change the format of the index. Then the
builder makes a full index of each repository on its own, at its next
pass. The run shows as a build after a push. You do not need to ask for
it.

## Ask for a new index

You need the right to change the repository: you are an organization admin,
or a member of the owner team.

```bash
kairos repos reindex <slug>
```

The builder makes a full index of the head of the default branch. The new
index replaces the index of that commit. The request goes before the
updates and the first builds that wait. When an update or a first build of
the repository waits, the request takes its place. The command shows the run. Follow
it with `kairos repos builds <slug>`.

In the GUI, the button "Build again" of the panel asks for the build. The
button is shown to a person with the right. An agent asks with the MCP
tool `rebuild_code_index`. The REST route is
`POST /api/repositories/{slug}/code-indexes/rebuild`, with no body.

## When the request is refused

| Code | Cause | What to do |
|---|---|---|
| `CODE_INDEX_BUILD_RUNNING` | A run of the repository is active, or a build of it runs now. | Wait for its end. `kairos repos builds <slug>` shows it. |
| `CODE_INDEX_BUILD_OFF` | The repository has the builder off. | Run `kairos repos update <slug> --code-index-build on`, then ask again. |
| `CODE_INDEX_BUILDER_OFF` | The deployment has no builder. | An operator sets `KAIROS_CODE_INDEX_DIR`. See [Configuration](../../reference/configuration/). |
| `FORBIDDEN` | You cannot change the repository. | Ask an organization admin, or a member of the owner team. |

## Stop the builds of a repository

A build can use much memory and processor time. To stop the builds of one
repository, set its code index builder to off. You need the right to change
the repository.

```bash
kairos repos update <slug> --code-index-build off
```

An agent uses the MCP tool `update_repository` with
`"code_index_build": "off"`. The REST call is:

```http
PATCH /api/repositories/{slug}
Content-Type: application/json

{"code_index_build": "off"}
```

When the builder is off:

- Kairos cancels the build jobs of the repository that wait. Their runs
  end as failed, with the text "cancelled: code_index_build was set to off".
- The builder adds no new job for the repository: no update after a push,
  and no first build.
- Kairos refuses a request for a new index (`CODE_INDEX_BUILD_OFF`).
- Kairos does not stop a build that runs now. When the build fails, it
  gets no further try.
- An upload of an index from a checkout still works.

`kairos repos show <slug>`, the MCP tool `get_repository` and
`GET /api/repositories/{slug}` show the value. To start the builds again,
set it to `on`.

## The breaker of a repository

A build job can stop its server: for example, the build runs out of memory
and the server stops. A build job can also stall: it makes no progress for
the lease time (`KAIROS_JOB_LEASE_SECS`). Kairos does not run that build
again, because it can stop the server again. The breaker of the
repository trips:

- The build job fails on its first try. Its run ends as failed.
- The builder adds no new job for the repository: no update after a push,
  and no first build.
- A build job of the repository that waits ends with the text "the code
  index breaker of the repository tripped". It does not build.
- An upload of an index from a checkout still works.

These show the breaker, the time it tripped and the reason:

- `kairos repos show <slug>`: the line `index breaker`.
- The MCP tool `get_repository`: the line `code index breaker`.
- `GET /api/repositories/{slug}`: the field `code_index_breaker`, with
  `tripped`, `tripped_at` and `reason`.
- The GUI page Admin, Code index: the panel Breakers lists each repository
  whose breaker tripped.

To reset the breaker, find the cause first (for example, give the server
more memory). Then ask for a build, as in
[Ask for a new index](#ask-for-a-new-index). The request resets the breaker,
and the builder works on the repository again.

Other jobs of the job queue (webhook deliveries, GitHub write-back, the
periodic passes) do not use a breaker. A job of these kinds whose server
stopped runs again while it has tries (`KAIROS_JOB_MAX_TRIES`).

## Related

- [Read a private repository](../read-a-private-repository/): the read
  token that the builder gives to git.
- [Configuration](../../reference/configuration/): the settings of the
  builder.
