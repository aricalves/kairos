// KAIROS-T-0578 — the item page is live, the merge dialog shows a diff, and a
// rollback sends the version that the page loaded.
//
// The cast: alice (org admin) has the item page open. The token of the spec
// is the other writer (an agent, a second tab).
//
//   1. the other writer saves the content: the banner "Changed on the server"
//      names the content; the draft of alice stays; the page does not mount
//      again (no new socket); Reload shows the new version and keeps the draft
//   2. the other writer writes the live state: the banner names the state
//   3. alice saves: the merge dialog shows a line diff; "Append my change
//      after theirs" puts her line after their text; she saves it
//   4. the history page: the other writer saves after the page loaded; a
//      rollback gets the 409 notice and overwrites nothing
//
// The task is new for each run (a timestamped title); the spec archives it
// at the end.

import { test, expect, type Page } from '@playwright/test';
import { mintToken } from '../helpers/auth';
import { createTask, getTask, loadBoard, patchTask } from '../helpers/api';

const GUI = process.env.E2E_GUI_BASE_URL ?? 'http://localhost:41080';
const BOARD = 'platform-delivery';

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

const editor = (page: Page) => page.locator('.kairos-editor');
const textarea = (page: Page) => page.locator('#kairos-editor-content');
const banner = (page: Page) => page.getByTestId('item-changed-banner');

test('the item page is live and the merge dialog appends', async ({ page }) => {
  const token = await mintToken();
  const board = await loadBoard(GUI, token, (b) => b.slug === BOARD);
  const task = await createTask(GUI, token, {
    title: `Live page e2e ${Date.now()}`,
    boardId: board.id,
    content: 'Intro.',
  });
  const code: string = task.short_code;

  await login(page);
  let sockets = 0;
  page.on('websocket', () => {
    sockets += 1;
  });
  await page.goto(`/items/${code}`);
  await expect(editor(page).getByText('Intro.')).toBeVisible();
  await expect.poll(() => sockets).toBeGreaterThan(0);

  const mine = 'Intro.\nMy line.';
  const theirs = 'Intro.\n\n## Note n-1\n\nTheir note.';

  await test.step('a save on the server shows the banner and keeps the draft', async () => {
    await editor(page).getByText('Edit', { exact: true }).click();
    await textarea(page).fill(mine);
    const socketsBefore = sockets;
    const before = await getTask(GUI, token, code);
    await patchTask(GUI, token, code, {
      title: before.title,
      content: theirs,
      version: before.version,
    });
    await expect(banner(page)).toBeVisible();
    await expect(banner(page)).toContainText('the content');
    await expect(textarea(page)).toHaveValue(mine);

    await banner(page).getByRole('button', { name: 'Reload' }).click();
    await expect(banner(page)).toHaveCount(0);
    await expect(page.locator('.cl-pill', { hasText: `v${before.version + 1}` }).first()).toBeVisible();
    await expect(textarea(page)).toHaveValue(mine);
    await expect(page.getByTestId('editor-draft-restored')).toHaveCount(0);
    expect(sockets).toBe(socketsBefore);
  });

  await test.step('a write of the state shows the banner with the state', async () => {
    const res = await fetch(`${GUI}/api/tasks/${code}/state`, {
      method: 'PUT',
      headers: { authorization: `Bearer ${token}`, 'content-type': 'application/json' },
      body: JSON.stringify({ next_step: 'Review the draft.' }),
    });
    expect(res.ok).toBeTruthy();
    await expect(banner(page)).toContainText('the state');
    await banner(page).getByRole('button', { name: 'Reload' }).click();
    await expect(banner(page)).toHaveCount(0);
    await expect(textarea(page)).toHaveValue(mine);
  });

  await test.step('the merge dialog shows a diff and appends my change', async () => {
    await editor(page).getByRole('button', { name: 'Save', exact: true }).click();
    const dialog = page.locator('[role="dialog"]');
    await expect(dialog.getByText('Edit conflict')).toBeVisible();
    const diff = dialog.getByTestId('merge-diff');
    await expect(diff.getByText('- Their note.')).toBeVisible();
    await expect(diff.getByText('+ My line.')).toBeVisible();
    await dialog.getByRole('button', { name: 'Append my change after theirs' }).click();
    await expect(dialog).toBeHidden();
    await expect(textarea(page)).toHaveValue(`${theirs}\n\nMy line.`);
    await expect(page.getByTestId('editor-appended')).toBeVisible();
    await editor(page).getByRole('button', { name: 'Save', exact: true }).click();
    await expect(page.locator('.cl-toast', { hasText: 'Saved' })).toBeVisible();
    // The save of alice is not a change on the server for her page.
    await expect(banner(page)).toHaveCount(0);
    const saved = await getTask(GUI, token, code);
    expect(saved.content).toBe(`${theirs}\n\nMy line.`);
  });

  await test.step('a rollback after a change on the server gets the 409', async () => {
    await page.goto(`/activity/history/${code}`);
    await expect(page.getByText('v1', { exact: true })).toBeVisible();
    const loaded = await getTask(GUI, token, code);
    await patchTask(GUI, token, code, {
      title: loaded.title,
      content: 'A save after the page loaded.',
      version: loaded.version,
    });
    const row = page.locator('tr', { has: page.getByText('v1', { exact: true }) });
    await row.getByRole('button', { name: 'Roll back' }).click();
    await page.locator('[role="dialog"]').getByRole('button', { name: 'Roll back' }).click();
    await expect(page.getByText('Version conflict (409)')).toBeVisible();
    const after = await getTask(GUI, token, code);
    expect(after.content).toBe('A save after the page loaded.');
  });

  // Put the task away: the seeded board stays as it was found.
  await fetch(`${GUI}/api/tasks/${code}`, {
    method: 'DELETE',
    headers: { authorization: `Bearer ${token}` },
  });
});
