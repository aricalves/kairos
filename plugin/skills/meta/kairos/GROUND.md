# Ground a plan in the record

Before you write a plan, read the record (COLLIERY-I-0627). A plan on a guess about the system is the failure this step prevents: the vision document, an ADR or the spec often says the opposite.

## The calls

1. `get_item` on the task. Follow the parent chain: the initiative, then its strategy. `get_item` each one. Their relationships list the documents that support them (a PRD, a spec, a research document): `get_item` each.
2. `get_repository <slug>` for the repository of this checkout (`repository` in the SessionStart context). It lists the documents and the ADRs that impact the repository. It also lists the ADRs of the owner team that no `impacts` link names. Most older ADRs have no link, so read that section too. `search` with `filter: {repository: <slug>, entity_type: [adr, document]}` finds the linked items only.
3. `team_pages <team>` for the owner team. It gives the tree of the documents and the ADRs of the team. Use it when `get_repository` says that it does not list each ADR ("N more"), and to find the documents of the team.
4. `related_work` on the task (and on the initiative when you plan the initiative). It gives proposals, not facts: `get_item` each one that looks related.
5. `get_item` on each hit that the first four calls gave. In an ADR, read the decision and its consequences. In a document, read the sections that the task touches.

## What to write

One sentence, before the plan, on the item and in the reply. Cite each item the way the Kairos rules say (a link when a deployment URL is wired).

- Nothing contradicts the plan: "Read: KAIROS-A-0028, COLLIERY-D-0013. Nothing read contradicts this plan."
- A step contradicts an ADR or a document: "Step 3 contradicts KAIROS-A-0028 (the decision in one clause): what the step does instead, and why." Then ask the user, or change the step. Do not plan around a decision in silence.
- Nothing to read: "Read: no ADR and no document impacts <slug>; the initiative has no document."

Name what you skipped: "Not checked: COLLIERY-D-0002 (40 pages; read the API section only)."
