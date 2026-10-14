// KAIROS-T-0087 — the KAIROS-I-0007 team landing pages smoke.
//
// One serial flow as bob (platform member, NOT a web member, NOT an org
// admin — so both the positive and negative permission gates are real):
//
//   1. REAL PKCE login via the Dex form (bob)
//   2. /teams/platform: the fixed v1 layout — seeded charter rendered,
//      announcements pinned-first, documentation tree with the diataxis
//      folders, the runbook as a document of the tree
//   3. post an announcement as bob (member) — it appears in the feed
//   4. tree-navigate to the seeded how-to page → edit → save
//   5. force a 409 via an API writer mid-edit → walk "take theirs"
//   6. charter shows content-edit only — no rename/move/delete controls
//   7. /teams/web (bob is NOT a member): no post box, no page-edit
//      affordance — reads stay open
//
// Conventions match team-lens.spec.ts: visible-text/role selectors plus
// stable `.kairos-*`/`.cl-*` classes; in-app navigation only (the SPA
// holds its token in memory — A-0015); the API writer mints its own PKCE
// token (helpers/auth).

import { test, expect, type Page } from '@playwright/test';
import { mintToken } from '../helpers/auth';
import { getTeamPage, patchTeamPage } from '../helpers/api';

const GUI = process.env.E2E_GUI_BASE_URL ?? 'http://localhost:41080';

const panel = (page: Page, title: string) =>
  page.locator('.cl-panel', {
    has: page.locator('.cl-panel__title', { hasText: title }),
  });

test('team pages: landing layout → announcements → page edit + 409 merge → charter protection → non-member negative', async ({
  page,
}) => {
  // 1. Real in-browser PKCE login (bob) ------------------------------------
  await test.step('login via Dex as bob (platform member, non-admin)', async () => {
    await page.goto('/');
    await page.waitForSelector('#login', { timeout: 30_000 });
    await page.fill('#login', 'bob@kairos.test');
    await page.fill('#password', 'bob-password');
    await page.click('#submit-login');
    await page.waitForURL((url) => url.pathname.startsWith('/boards'), {
      timeout: 30_000,
    });
  });

  // 2. The fixed v1 landing layout -----------------------------------------
  await test.step('platform landing: charter, announcements, tree, work documents', async () => {
    await page
      .locator('.kairos-nav__section')
      .getByRole('link', { name: 'Platform', exact: true })
      .click();
    await page.waitForURL(/\/teams\/platform$/);

    // Charter renders through the markdown pipeline (seeded content).
    const charter = panel(page, 'Charter');
    await expect(charter.locator('.kairos-markdown h2', { hasText: 'Mission' })).toBeVisible();
    await expect(charter.getByText('paved road', { exact: false })).toBeVisible();

    // Announcements: the seeded pinned welcome sits first.
    const announcements = panel(page, 'Announcements');
    await expect(announcements.locator('.cl-pill', { hasText: 'pinned' })).toBeVisible();
    await expect(
      announcements.getByText('Welcome to the Platform team space', { exact: false }),
    ).toBeVisible();
    // One-way by design: no comment or reaction affordance anywhere.
    await expect(announcements.getByRole('button', { name: /comment|react/i })).toHaveCount(0);

    // Documentation tree: the folders the team made (KAIROS-T-0436: no
    // seeded folder set), nested.
    const docs = panel(page, 'Documentation');
    await expect(docs.getByText('Support Processes')).toHaveCount(0);
    const docsFolder = docs.locator('details.kairos-doctree__folder', {
      has: page.locator('summary', { hasText: 'Documentation' }),
    });
    await docsFolder.locator('summary').first().click();
    const howTo = docsFolder.locator('details.kairos-doctree__folder', {
      has: page.locator('summary', { hasText: 'How-to Guides' }),
    });
    await howTo.locator('summary').click();
    await expect(
      howTo.getByRole('link', { name: 'Deploy Kairos', exact: true }),
    ).toBeVisible();

    // KAIROS-T-0437: the documents of the team are nodes of its tree; the
    // "Work documents" panel is gone. The seeded runbook (owned by the
    // platform board) is at the root; the PRD (owned by the initiative
    // board) is not the team's.
    await expect(panel(page, 'Work documents')).toHaveCount(0);
    const tree = panel(page, 'Documentation');
    const runbook = tree.locator('.kairos-doctree__item', {
      hasText: 'Runbook: password-less auth rollout',
    });
    await expect(runbook).toBeVisible();
    await expect(runbook.getByText('doc', { exact: true })).toBeVisible();
    await expect(tree.getByText('PRD: Portal sign-up flow')).toHaveCount(0);
  });

  // 3. The tree grows in place (KAIROS-T-0094, KAIROS-T-0438) -------------
  // A member makes a folder at the root of the tree, where they are: the
  // slug comes from the title. Then they reorder and move it by drag and
  // drop.
  await test.step('member grows the tree in place: create, reorder, move', async () => {
    const docs = panel(page, 'Documentation');
    // The outermost list is the root of the tree; nested lists are inside it.
    const rootList = docs.locator('.kairos-doctree__list').first();
    const rootSummaries = rootList.locator(':scope > details > summary');
    // Folders are closed, so the only visible "+ Folder" is the root's.
    await docs.getByRole('button', { name: '+ Folder', exact: true }).last().click();
    await docs.getByLabel('New folder').fill('Incidents');
    await expect(docs.getByLabel('Slug')).toHaveValue('incidents');
    await docs.getByRole('button', { name: 'Create', exact: true }).click();

    // It lands as a sibling of Documentation, not nested inside it.
    const incidents = docs.locator('details.kairos-doctree__folder', {
      has: page.locator('summary', { hasText: 'Incidents' }),
    });
    await expect(incidents).toBeVisible();
    await expect(rootSummaries.last()).toContainText('Incidents');

    // A duplicate root slug surfaces the server's typed 422 rather than a
    // silent no-op.
    await docs.getByRole('button', { name: '+ Folder', exact: true }).last().click();
    await docs.getByLabel('New folder').fill('Incidents');
    await docs.getByRole('button', { name: 'Create', exact: true }).click();
    await expect(docs.getByText('Create failed')).toBeVisible();
    await docs.getByRole('button', { name: 'Cancel', exact: true }).click();

    // The tree keeps the folders a person opened (step 2 opened
    // Documentation). Close it, so each drag stays short and on screen; a
    // drop into a folder opens it again.
    const closeFolder = async (title: string) => {
      await rootSummaries.filter({ hasText: title }).first().evaluate((summary) => {
        const details = summary.parentElement as HTMLDetailsElement;
        if (details.open) details.open = false;
      });
    };
    await closeFolder('Documentation');

    // Reorder: drag Incidents onto the gap before the first root folder.
    await incidents.locator('summary').first().dragTo(
      rootList.locator(':scope > .kairos-doctree__gap').first(),
    );
    await expect(rootSummaries.first()).toContainText('Incidents');

    // Move: drag Incidents onto the Documentation folder.
    const documentation = docs.locator('details.kairos-doctree__folder', {
      has: page.locator('summary', { hasText: 'Documentation' }),
    }).first();
    await docs.locator('summary', { hasText: 'Incidents' }).first()
      .dragTo(documentation.locator('summary').first());
    // The folder that takes the node opens, and stays open when the tree
    // renders again.
    await expect(rootSummaries.filter({ hasText: 'Incidents' })).toHaveCount(0);
    await expect(
      documentation.locator('details.kairos-doctree__folder summary', { hasText: 'Incidents' }),
    ).toBeVisible();

    // KAIROS-T-0437: a document drags into a folder like a page; its board
    // keeps it.
    await closeFolder('Documentation');
    await docs.locator('.kairos-doctree__item', { hasText: 'Runbook: password-less auth rollout' })
      .first()
      .dragTo(documentation.locator('summary').first());
    await expect(
      rootList.locator(':scope > .kairos-doctree__item', { hasText: 'Runbook: password-less auth rollout' }),
    ).toHaveCount(0);
    await expect(
      documentation.locator('.kairos-doctree__item', { hasText: 'Runbook: password-less auth rollout' }),
    ).toBeVisible();
  });

  await test.step('the empty pages path is the root of the tree', async () => {
    await page.goto('/teams/platform/pages/');
    await expect(panel(page, 'Pages')).toBeVisible();
    await expect(page.getByText('Page not found')).toHaveCount(0);
  });

  // KAIROS-T-0439: the history of a page names who saved each version.
  await test.step('member reads the history of a page', async () => {
    await page.goto('/teams/platform/pages/documentation/how-to-guides/deploy-kairos');
    const history = panel(page, 'History');
    await history.getByRole('button', { name: 'Show history', exact: true }).click();
    // The seed saved it once; the edits of this spec come later.
    await expect(history.getByText(/^v1 · /)).toBeVisible();
  });

  // KAIROS-T-0439: a folder is archived with what is in it, and comes back
  // with it.
  await test.step('member archives a folder and restores it from the archive', async () => {
    await page.goto('/teams/platform/pages/documentation/incidents');
    await panel(page, 'Manage').getByRole('button', { name: 'Archive', exact: true }).click();
    const dialog = page.getByRole('dialog');
    await expect(dialog.getByText('Archive this folder?')).toBeVisible();
    await dialog.getByRole('button', { name: 'Archive', exact: true }).click();
    await page.waitForURL(/\/teams\/platform\/pages\/documentation$/);

    await page.goto('/teams/platform/pages/');
    const archived = panel(page, 'Archived');
    await expect(archived.getByText('Incidents', { exact: true })).toBeVisible();
    await archived.getByRole('button', { name: 'Restore', exact: true }).first().click();
    await expect(archived.getByText('Nothing is archived.')).toBeVisible();
    await page.goto('/teams/platform/pages/documentation/incidents');
    await expect(page.getByText('Page not found')).toHaveCount(0);
    await page.goto('/teams/platform');
  });

  // 4. Post an announcement as a team member -------------------------------
  const posted = `Bob's standup note ${Date.now()}`;
  await test.step('member posts an announcement', async () => {
    const announcements = panel(page, 'Announcements');
    await announcements.locator('textarea').fill(posted);
    await announcements.getByRole('button', { name: 'Post' }).click();
    await expect(announcements.getByText(posted)).toBeVisible({ timeout: 15_000 });
    // Pinned still first: the welcome stays above bob's fresh post.
    const bodies = announcements.locator('.kairos-markdown');
    await expect(bodies.first()).toContainText('Welcome to the Platform team space');
  });

  // 4. Tree → page → edit → save -------------------------------------------
  await test.step('open the how-to page and save an edit', async () => {
    // The announcement post refetched the page; whether the re-render
    // preserved the disclosure state is a DOM-reuse detail — force both
    // folders open instead of toggling blindly (a click on an
    // already-open <details> would CLOSE it).
    const docs = panel(page, 'Documentation');
    const docsFolder = docs.locator('details.kairos-doctree__folder', {
      has: page.locator('summary', { hasText: 'Documentation' }),
    });
    await docsFolder.evaluate((el) => ((el as HTMLDetailsElement).open = true));
    const howTo = docsFolder.locator('details.kairos-doctree__folder', {
      has: page.locator('summary', { hasText: 'How-to Guides' }),
    });
    await howTo.evaluate((el) => ((el as HTMLDetailsElement).open = true));
    await howTo
      .getByRole('link', { name: 'Deploy Kairos', exact: true })
      .click();
    await page.waitForURL(/\/teams\/platform\/pages\/documentation\/how-to-guides\/deploy-kairos$/);

    // Breadcrumbs walk the ancestry; content rendered via the pipeline.
    await expect(page.getByRole('link', { name: 'Platform', exact: true })).toBeVisible();
    await expect(page.getByRole('link', { name: 'How-to Guides', exact: true })).toBeVisible();
    const content = panel(page, 'Content');
    await expect(content.locator('.kairos-markdown')).toContainText('angreal services up');

    await content.getByRole('button', { name: 'Edit', exact: true }).click();
    const editor = page.locator('.kairos-editor');
    const textarea = editor.locator('textarea.kairos-editor__textarea');
    await expect(textarea).toBeVisible();
    // The markdown toolbar inserts syntax at the cursor.
    await textarea.click();
    await editor.getByTitle('Bold', { exact: true }).click();
    await expect(textarea).toHaveValue(/\*\*bold\*\*/);

    await textarea.fill(`# Deploy Kairos\n\nEdited by the teampages smoke ${Date.now()}`);
    await editor.getByRole('button', { name: 'Save' }).click();
    // Save returns to view mode with the refetched v2.
    await expect(panel(page, 'Content').locator('.kairos-markdown')).toContainText(
      'Edited by the teampages smoke',
      { timeout: 15_000 },
    );
    await expect(panel(page, 'Content').locator('.cl-pill', { hasText: 'v2' })).toBeVisible();
  });

  // 5. Force a 409 mid-edit; walk "take theirs" ----------------------------
  await test.step('API writer forces a 409; take-theirs adopts the server copy', async () => {
    const token = await mintToken({ server: GUI }); // alice, org admin
    const saved = await getTeamPage(
      GUI,
      token,
      'platform',
      'documentation/how-to-guides/deploy-kairos',
    );

    await panel(page, 'Content').getByRole('button', { name: 'Edit', exact: true }).click();
    const editor = page.locator('.kairos-editor');
    const textarea = editor.locator('textarea.kairos-editor__textarea');
    await textarea.fill(`My conflicting edit ${Date.now()}`);

    const serverContent = `Server won the page ${Date.now()}`;
    await patchTeamPage(GUI, token, saved.teamId, saved.id, {
      title: saved.title,
      content: serverContent,
      version: saved.version,
    });

    await editor.getByRole('button', { name: 'Save' }).click();
    const dialog = page.locator('[role="dialog"]');
    await expect(dialog).toBeVisible();
    await expect(dialog.getByText('Edit conflict')).toBeVisible();
    await expect(dialog.getByText(`server is at v${saved.version + 1}`)).toBeVisible();
    await dialog.getByRole('button', { name: 'Take theirs' }).click();
    await expect(dialog).toBeHidden();
    await expect(textarea).toHaveValue(serverContent);
  });

  // 6. Charter protection: content edit only -------------------------------
  await test.step('charter offers no destructive controls', async () => {
    await page.getByRole('link', { name: 'Platform', exact: true }).first().click();
    await page.waitForURL(/\/teams\/platform$/);
    await panel(page, 'Charter')
      .getByRole('link', { name: 'Open / edit the charter' })
      .click();
    await page.waitForURL(/\/teams\/platform\/pages\/charter$/);

    // Editable (bob is a member) …
    await expect(
      panel(page, 'Content').getByRole('button', { name: 'Edit', exact: true }),
    ).toBeVisible();
    // … but protected: no Manage panel, no rename/move/delete anywhere.
    await expect(panel(page, 'Manage')).toHaveCount(0);
    await expect(page.getByRole('button', { name: 'Rename' })).toHaveCount(0);
    await expect(page.getByRole('button', { name: 'Archive' })).toHaveCount(0);
    await expect(page.getByText('The charter is protected', { exact: false })).toBeVisible();
  });

  // 7. Non-member negative: web's page is read-only for bob ----------------
  await test.step('web team page: reads open, no write affordances for bob', async () => {
    await page
      .locator('.cl-appshell__navbar')
      .getByRole('link', { name: 'Teams', exact: true })
      .click();
    await page.waitForURL(/\/teams$/);
    await page.locator('.kairos-board-tile', { hasText: 'Web' }).click();
    await page.waitForURL(/\/teams\/web$/);

    // Reads are open tenant-wide: charter + the seeded announcement show.
    await expect(
      panel(page, 'Charter').locator('.kairos-markdown h2', { hasText: 'Mission' }),
    ).toBeVisible();
    const announcements = panel(page, 'Announcements');
    await expect(
      announcements.getByText('Sprint demo Friday', { exact: false }),
    ).toBeVisible();
    // But no post box, and no edit affordance on web's pages.
    await expect(announcements.locator('textarea')).toHaveCount(0);
    await expect(announcements.getByRole('button', { name: 'Post' })).toHaveCount(0);
    // KAIROS-T-0094, KAIROS-T-0438: and no create in the tree either. A
    // non-member sees no affordance rather than a button that 403s.
    await expect(panel(page, 'Documentation').getByRole('button', { name: '+ Page' })).toHaveCount(0);
    await expect(panel(page, 'Documentation').getByRole('button', { name: '+ Folder' })).toHaveCount(0);
    await panel(page, 'Charter')
      .getByRole('link', { name: 'Open / edit the charter' })
      .click();
    await page.waitForURL(/\/teams\/web\/pages\/charter$/);
    await expect(
      panel(page, 'Content').getByRole('button', { name: 'Edit', exact: true }),
    ).toHaveCount(0);
    await expect(panel(page, 'Manage')).toHaveCount(0);
  });
});
