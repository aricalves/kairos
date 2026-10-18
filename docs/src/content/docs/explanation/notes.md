---
title: "Notes: a record next to the content"
---

A note records work on an item: a finding, a decision, a dead end or the
next step. People and agents write notes with `append_note`, the REST route
`POST /api/{family}/{code}/notes` or `kairos <noun> note`. This page tells
how Kairos keeps notes, and why.

## A note is a row of its own

Each note is one row of the table `item_notes`. The row has these parts:

- the id of the note in its item: `n-3`;
- the author: a user of the organization;
- the time of the note;
- the markdown text;
- the mark of a promote: the code of the item that the note became.

The content of the item does not hold the notes. Thus a note writes no
version of the content and no row of the history. An edit of the content
does not change a note. A note does not refuse an edit of the content
either.

## How Kairos shows the notes

`get_item` shows the notes after the content. Each note has a heading with
its id, its date and its author:

```text
## Note n-3 (2026-10-11, Robin)

The text of the note.
```

`get_item` also lists the notes after the relationships, one line for each
note. The REST read of one item has the array `notes`, and `content` has no
note. `kairos <noun> get` prints the notes after the content. The item page
of the GUI shows each note with an anchor, for example `#n-3`.

Cite a note by its id: "note n-3 on ACME-T-0012". Kairos never gives an id
again, so a citation stays true.

## Any markdown is a note

The text of a note can hold any markdown, `## ` headings too. A heading does
not end the note, because the row ends the note. Each `@user_name` in the
text is a mention, as in the content.

## A promote

`promote_note` makes a draft ADR or document from a note. The new item gets
the text of the note and a line that names the note. The note gets the mark
of the new item, and `get_item` shows it as `Promoted to ACME-A-0029.`

Kairos promotes a note one time only. A second promote gets a `CONFLICT`
refusal, and the refusal names the item that the note became.

## Search and retrieval

The text of each note is in the full-text vector of its item. Thus `search`
finds an item by a word of one of its notes. The embedding sweep reads the
content and then the notes, as `## Note` sections.

## Events

A note sends the event `item_updated`, and then `note_appended` with the id
of the note. A promote sends `item_updated`, and then `note_promoted`. A
board view reads the card again for each of these events.

## Why notes are not part of the content

Before KAIROS-T-0630, `append_note` added a `## Note` section to the content.
Each note thus wrote a full copy of the content to the history. The history
grew with the square of the count of notes. One hundred notes of 1 KB on
one item made about 5 MB of history.

Notes in the content had other problems too:

- A `## ` heading in the text of a note ended the note.
- The id of a new note came from the count of the notes. A hand edit that
  removed a note gave an id a second time.
- A note made a new version, so a person who edited the item at the same
  time got a conflict.
- A second promote of a note made a second item.

The decision is KAIROS-A-0054.

## The move of the old notes

The tenant migration `2026-10-18-003000_item_notes` moved each old note into
a row, one time. The migration read the content as Kairos read it before:

- A note kept its id when no earlier note of the item had that id.
- A note with no id got the id of its position, as `get_item` showed it.
- A note with the id of an earlier note got the next free id. A last line
  of its text names the old id.
- The name in the heading became the user with that name, email or user
  name. A name with no user of the organization stays a name.
- A last line `Promoted to CODE.` became the mark of the promote.

The migration removed the notes from the content. It wrote no version and
no row of the history. So the newest version in the history still has the
notes.

Kairos cannot repair an old note that a stray `## ` heading cut. The text
after that heading stayed in the content, and the note has only the text
before it.
