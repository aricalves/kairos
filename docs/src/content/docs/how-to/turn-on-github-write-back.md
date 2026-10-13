---
title: "Turn on GitHub write-back"
---

This page describes Kairos 0.12.0.

Show the state of a Kairos item on its GitHub pull requests. With a write
token on the repository, Kairos writes 2 things on each pull request that
links to an item:

- One comment for each item. The comment has the code, the title, the column,
  the claim and a link to the item. Kairos edits the same comment when the
  item changes.
- The commit status `kairos` on the head commit of the pull request:
  - `success`: each item is done.
  - `failure`: a person cancelled an item.
  - `pending`: all other cases.

A pull request links to an item when its title, body or branch has the code
of the item. See [Connect a git forge](../connect-a-git-forge/).

**Before you start:**

- The repository is a GitHub repository in Kairos, with a forge connection.
- You are an organization admin, or a member of the owner team of the
  repository.
- The deployment has the setting `KAIROS_SECRETS_KEY`. See
  [Read a private repository](../read-a-private-repository/#1-set-the-key-of-the-deployment).

## 1. Make a write token on GitHub

Use a GitHub fine-grained personal access token:

1. On GitHub, open **Settings**, then **Developer settings**, then
   **Fine-grained tokens**.
2. Select **Generate new token**.
3. For **Repository access**, select **Only select repositories**. Select the
   one repository.
4. For **Permissions**, give **Pull requests** the value **Read and write**.
   Give **Commit statuses** the value **Read and write**. Give no other
   permission.
5. Copy the token.

The comments and the statuses show the name of the owner of the token. Use
the token of a different GitHub account if you do not want your name on them.

A token of a GitHub App is a later option. This version takes a personal
token only.

## 2. Set the token in Kairos

With the CLI, send the token on standard input:

```sh
printf '%s' "$TOKEN" | kairos repos write-token set site
```

Or use the GUI: open **Admin**, then **Repositories**. Select **Edit** on the
repository, put the token in **Write token**, and select **Set write token**.

Or use REST: `PUT /api/repositories/{slug}/write-token` with
`{"token": "..."}`.

Kairos keeps the token encrypted. No command, page or tool shows it again.
Kairos then writes each open pull request that links to an item.

## 3. Look at the result

```sh
kairos repos write-token status site
# → write-back: on (token set by Robin at 2026-10-12T09:15:00+00:00; the last write at 2026-10-12T09:15:04+00:00 worked)
```

The repository page of the GUI and the MCP tool `get_repository` show the
same line. When a write fails, the line has the error, and Kairos tries the
write again with longer waits. A change of an item does not wait for GitHub.

GitHub can refuse the token: `401` (a bad token) or `403` (a missing
permission). Then Kairos does not try that write again. Make a new token,
and set it again.

## 4. Turn it off

```sh
kairos repos write-token remove site
```

Kairos writes nothing more. The comments and the statuses stay on GitHub.

## Related reading

- [Connect a git forge](../connect-a-git-forge/): the links between pull
  requests and items.
- [Configuration](../../reference/configuration/#github-write-back): the
  settings.
- [CLI](../../reference/cli/#kairos-repos-write-token): the commands.
