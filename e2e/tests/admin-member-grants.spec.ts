// KAIROS-T-0586 — Admin → Members → a person: the grid of the grants of
// one person on many boards, and the teams of the person (KAIROS-T-0413).
//
//   1. Members lists carol; her name opens her grants page;
//   2. a tick on one cell counts one changed cell; Undo the changes puts it
//      back; a tick and Save grants it, and the server has it; an untick
//      and Save revokes it;
//   3. the All boards row ticks a column on each board where it applies,
//      and Undo the changes clears it again (nothing is saved);
//   4. the Teams panel adds carol to a team and removes her again.
//
// Each test puts back what it changed, also when it fails.

import { test, expect, type Page } from '@playwright/test';
import { mintToken } from '../helpers/auth';
import { readEachPage } from '../helpers/api';

const GUI = process.env.E2E_GUI_BASE_URL ?? 'http://localhost:41080';
const BOARD = 'platform-delivery';
const CAPABILITY = 'approve_proposals';

async function login(page: Page): Promise<void> {
  await page.goto('/');
  await page.waitForSelector('#login', { timeout: 30_000 });
  await page.fill('#login', 'alice@kairos.test');
  await page.fill('#password', 'alice-password');
  await page.click('#submit-login');
  await page.waitForURL((url) => url.pathname === '/' || url.pathname.startsWith('/boards'), {
    timeout: 30_000,
  });
}

async function call(token: string, method: string, path: string, body?: unknown): Promise<any> {
  const res = await fetch(GUI + path, {
    method,
    headers: { authorization: `Bearer ${token}`, 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await res.text();
  if (!res.ok) throw new Error(`${method} ${path} -> ${res.status}: ${text}`);
  return text ? JSON.parse(text) : null;
}

async function carol(token: string): Promise<any> {
  const members = await readEachPage(GUI, token, '/api/members');
  const found = members.find((m) => m.email === 'carol@kairos.test');
  expect(found, 'carol is a member').toBeTruthy();
  return found;
}

/** The capabilities of carol on a board, from the server. */
async function grantsOn(token: string, userId: string, slug: string): Promise<string[]> {
  const grants = await call(token, 'GET', `/api/members/${userId}/grants`);
  return (grants.boards as any[]).find((b) => b.board_slug === slug)?.capabilities ?? [];
}

const changed = (page: Page) =>
  page
    .locator('.cl-panel', { has: page.locator('.cl-panel__title', { hasText: 'Boards' }) })
    .locator('.cl-group', { hasText: /No change\.|changed cells?\./ })
    .last();

test('member grants: one cell, the All boards row, and Undo', async ({ page }) => {
  // Each page load compiles the wasm bundle of the app again.
  test.setTimeout(180_000);
  const token = await mintToken();
  const person = await carol(token);
  const board = (await call(token, 'GET', `/api/boards/${BOARD}`)).id as string;
  const notice = page.locator('.cl-alert', { hasText: 'Done' });
  const cell = page.getByLabel(`Proposals on ${BOARD}`, { exact: true });
  expect(await grantsOn(token, person.user_id, BOARD)).not.toContain(CAPABILITY);

  try {
    await test.step('Members: the name of carol opens her grants page', async () => {
      await login(page);
      await page.goto('/admin/members');
      await page.getByRole('link', { name: person.display_name, exact: true }).click();
      await page.waitForURL(new RegExp(`/admin/members/${person.user_id}$`));
      await expect(page.locator('.cl-page-header, h1', { hasText: person.display_name }).first())
        .toBeVisible({ timeout: 30_000 });
      await expect(cell).not.toBeChecked();
      await expect(changed(page)).toContainText('No change.');
    });

    await test.step('one tick is one changed cell; Undo puts it back', async () => {
      await cell.check();
      await expect(changed(page)).toContainText('1 changed cell.');
      await page.getByRole('button', { name: 'Undo the changes', exact: true }).click();
      await expect(cell).not.toBeChecked();
      await expect(changed(page)).toContainText('No change.');
    });

    await test.step('a tick and Save grants it; an untick and Save revokes it', async () => {
      await cell.check();
      await page.getByRole('button', { name: 'Save', exact: true }).click();
      await expect(notice).toContainText('Kairos changed 1 grants on 1 boards.');
      await expect.poll(() => grantsOn(token, person.user_id, BOARD)).toContain(CAPABILITY);
      await expect(cell).toBeChecked();
      await cell.uncheck();
      await page.getByRole('button', { name: 'Save', exact: true }).click();
      // The notice of the first Save is still there: ask the server.
      await expect
        .poll(() => grantsOn(token, person.user_id, BOARD))
        .not.toContain(CAPABILITY);
      await expect(changed(page)).toContainText('No change.');
    });

    await test.step('the All boards row ticks the column on each board; Undo', async () => {
      const grants = await call(token, 'GET', `/api/members/${person.user_id}/grants`);
      const applies = (grants.boards as any[]).filter((b) => b.applicable.includes(CAPABILITY));
      const fresh = applies.filter((b) => !b.capabilities.includes(CAPABILITY)).length;
      expect(fresh, 'boards where carol has no Proposals grant').toBeGreaterThan(1);
      await page.getByLabel('Proposals on all boards', { exact: true }).click();
      for (const b of applies) {
        await expect(page.getByLabel(`Proposals on ${b.board_slug}`, { exact: true })).toBeChecked();
      }
      await expect(changed(page)).toContainText(`${fresh} changed cells.`);
      await page.getByRole('button', { name: 'Undo the changes', exact: true }).click();
      await expect(changed(page)).toContainText('No change.');
      await expect(cell).not.toBeChecked();
    });
  } finally {
    await call(token, 'PATCH', `/api/members/${person.user_id}/grants`, {
      changes: [{ board_id: board, grant: [], revoke: [CAPABILITY] }],
    }).catch(() => undefined);
  }
});

test('member grants: the Teams panel adds and removes the person', async ({ page }) => {
  test.setTimeout(180_000);
  const token = await mintToken();
  const person = await carol(token);
  const teams = await readEachPage(GUI, token, '/api/teams');
  const platform = teams.find((t) => t.slug === 'platform');
  expect(platform, 'the team platform').toBeTruthy();
  const panel = page.locator('.cl-panel', {
    has: page.locator('.cl-panel__title', { hasText: 'Teams' }),
  });
  const teamsOf = async () =>
    ((await call(token, 'GET', `/api/members/${person.user_id}/grants`)).teams as any[]).map(
      (t) => t.slug,
    );
  expect(await teamsOf()).not.toContain('platform');

  try {
    await login(page);
    await page.goto(`/admin/members/${person.user_id}`);
    await expect(panel).toBeVisible({ timeout: 30_000 });
    await panel.getByLabel('Team').selectOption({ label: platform.name });
    await panel.getByRole('button', { name: 'Add to the team', exact: true }).click();
    await expect(page.locator('.cl-alert', { hasText: 'Done' })).toContainText(
      'Kairos added the person to the team.',
    );
    expect(await teamsOf()).toContain('platform');
    await panel
      .locator('.cl-group', { hasText: platform.name })
      .filter({ has: page.getByRole('button', { name: 'Remove from the team' }) })
      .last()
      .getByRole('button', { name: 'Remove from the team', exact: true })
      .click();
    await expect(page.locator('.cl-alert', { hasText: 'Done' })).toContainText(
      'Kairos removed the person from the team.',
    );
    expect(await teamsOf()).not.toContain('platform');
  } finally {
    if ((await teamsOf()).includes('platform')) {
      await call(token, 'DELETE', `/api/teams/${platform.id}/members/${person.user_id}`);
    }
  }
});
