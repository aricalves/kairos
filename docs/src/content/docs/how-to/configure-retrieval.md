---
title: "Configure semantic retrieval"
---

Make Kairos able to find work that is *about* the same thing, not only work that
uses the same words. This guide assumes you can set environment variables on the
deployment, or Helm values if you installed with the chart.

You need: a PostgreSQL with `pgvector` (Kairos requires it anyway — see
[Install with Helm](../install-with-helm/)).

## It already works

The container ships the model inside it. With no configuration at all, retrieval
runs locally: nothing leaves your network, and there is no API key to manage.

Check by asking any deployment:

```sh
curl -H "Authorization: Bearer $TOKEN" \
  https://<kairos-host>/api/items/<SHORT-CODE>/related
```

`"vector": true` in the response means it is working. `"vector": false` means it
answered from text alone — see [When it answers from text
only](#when-it-answers-from-text-only).

## How new work gets its vectors

A background sweep makes the vectors. It does not slow down a write.

1. A write marks its item. A create, an edit, a note, a new parent and new
   metadata all mark the item.
2. Some writes also mark other items. A new title marks the children of the
   item. A new repository slug marks the tasks of the repository. A new
   name of a metadata field marks each item with a value of that field.
3. The sweep reads the marked items, the oldest first. It reads no other item.
4. The sweep compares the hash of each text with the stored hash. It sends
   only changed text to the model.
5. The sweep writes the vectors and removes the marks. If a write changes an
   item during this step, the item keeps its mark. The next run reads the new
   text.

Each organization has one job for the sweep. After each run, the job starts
again. It waits `KAIROS_EMBED_REFRESH_SECS` (10 by default), or it starts
immediately when the run had a full page. Thus a new item gets its vector in
about 10 seconds. `KAIROS_EMBED_REFRESH_SECS=0` stops the sweep.

When a server starts, the sweep marks each item that has no vector of the
configured model. Thus the sweep also fills in items after a new model.

## Fill in the work you already have

The sweep fills in old work too, but slowly, one page at a time. To fill in
old work faster, run the backfill once:

```sh
kairos-server embed-backfill
```

It prints what it did, per tenant:

```
backfilling with local/bge-small-en-v1.5-q (384d)
demo: 20 item(s) updated, 41 text(s) embedded in 0.4s — 20/20 items now current
```

It is safe to interrupt and safe to re-run: it asks the database what is still
missing each time, so there is no cursor to lose. Running it on an already-current
tenant does nothing.

For a large tenant, be kind to whatever is doing the embedding:

```sh
kairos-server embed-backfill --batch 50 --pause-ms 200 --max-batches 100
```

## Build the index

Once vectors exist, build the approximate-search index:

```sh
kairos-server embed-index
```

This is a **table rewrite and an index build** — schedule it like any other.
Idempotent: a second run reports `already pinned and indexed`.

Do this *after* the backfill, not before. It refuses to run if stored vectors
disagree with the configured model, and tells you to re-embed first.

## Use a hosted model instead

Point Kairos at anything speaking the OpenAI embeddings shape — OpenAI, Azure
OpenAI, a local Ollama, a vLLM server:

```yaml
embeddings:
  url: "https://api.openai.com/v1"
  model: "text-embedding-3-small"
  existingSecret: "kairos-embed"   # holding KAIROS_EMBED_API_KEY
```

Or as environment variables: `KAIROS_EMBED_URL`, `KAIROS_EMBED_MODEL`,
`KAIROS_EMBED_API_KEY`. Setting the URL selects the remote provider on its own; a
local Ollama needs no key.

**A new model makes every stored vector old.** You cannot compare vectors from
two models, also at the same width. Thus Kairos ignores the vectors of the old
model. It does not compare across models. After a change of the model, the
sweep marks every item when the server starts. To go faster, run these
commands:

```sh
kairos-server embed-backfill   # re-embed everything under the new model
kairos-server embed-index      # re-pin the columns and rebuild
```

Until that finishes, retrieval answers from text alone and says so. It does not
return wrong answers in the meantime.

## Turn it off

```yaml
embeddings:
  provider: none
```

Search still works. `related_work` says it is not enabled and points at ordinary
search; `/api/items/{code}/related` returns **503** rather than an empty list,
because an empty list would read as "nothing is related".

## When it answers from text only

`"vector": false`, or a note saying *"Text only"*. Three causes, in the order
worth checking:

1. **The sweep made no vector yet.** Wait for the sweep, or run the backfill
   above.
2. **The model changed** and the stored vectors are from the old one. The
   backfill fixes it; `embed-backfill` prints `N from another model` when this is
   the cause.
3. **The provider will not start.** The server logs a warning at startup and
   keeps serving. For a remote endpoint, that is usually the URL, the key, or the
   model name. For the local model in a source build rather than the container,
   it is a missing model cache — `angreal dev fetch-model` fills it.

In every case search keeps working and the answers stay honest about being thin.

A text-only answer gives 3 proposals at most. Each proposal must match a large
share of the rare words of the title. When no item does, the answer is `None
found`. The rules are in [`related_work`](../../reference/mcp-tools/#related_work).

## Watch the suggestions

Retrieval suggests edges; people confirm them. Whether those suggestions are any
good is visible in how often they are accepted — a confirm rate drifting toward
zero means the suggestions are not recognisable, which is worth knowing before
everyone starts ignoring them.

## Related reading

- [Finding related work](../../explanation/finding-related-work/) — why the
  results are proposals, and why there is no threshold to tune
- [Install with Helm](../install-with-helm/) — the `pgvector` requirement and the
  bundled evaluation database
- [Configuration](../../reference/configuration/) — every `KAIROS_EMBED_*`
  variable and chart value
