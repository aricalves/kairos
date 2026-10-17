// KAIROS-T-0581 — who may edit, on the item page, and a draft that
// survives a panel action and a reload.
//
// The cast: alice is an org admin. carol is a member of team web, with
// nothing on the board of team platform, so she may not edit a task that
// alice made there.
//
//   1. carol sees the content read-only, with the reason; no Edit tab, no
//      Archive, Save and Save metadata disabled, with the reason
//   2. alice sees Archive, and types a draft
//   3. a panel action (Set lane) keeps the page mounted (KAIROS-T-0618);
//      the draft stays
//   4. a reload of the tab restores the draft; Discard draft drops it
//   5. a save removes the stored draft: the next reload shows no draft
//
// The task is new for each run (a timestamped title); the spec archives
// it at the end.

import { test, expect, type Browser, type Page } from '@playwright/test';
import { mintToken } from '../helpers/auth';
import { createTask, loadBoard } from '../helpers/api';

const GUI = process.env.E2E_GUI_BASE_URL ?? 'http://localhost:41080';
const BOARD = 'platform-delivery';

/** A browser of its own for one person, signed in through Dex. */
async function signIn(browser: Browser, name: string): Promise<Page> {
  const context = await browser.newContext({ baseURL: GUI });
  const page = await context.newPage();
  await page.goto('/');
  await page.waitForSelector('#login', { timeout: 30_000 });
  await page.fill('#login', `${name}@kairos.test`);
  await page.fill('#password', `${name}-password`);
  await page.click('#submit-login');
  await page.waitForURL((url) => url.pathname === '/' || url.pathname.startsWith('/boards'), {
    timeout: 30_000,
  });
  // whoami resolved: each "is not there" below is by the rule.
  await expect(
    page.locator('.cl-appshell__header').getByText(name, { exact: true }),
  ).toBeVisible();
  return page;
}

const editor = (page: Page) => page.locator('.kairos-editor');
const textarea = (page: Page) => page.locator('#kairos-editor-content');

test('the item page is read-only with a reason, and a draft survives', async ({ browser }) => {
  const token = await mintToken();
  const board = await loadBoard(GUI, token, (b) => b.slug === BOARD);
  const task = await createTask(GUI, token, {
    title: `Edit access e2e ${Date.now()}`,
    boardId: board.id,
    content: 'The server copy.',
  });
  const code: string = task.short_code;

  await test.step('carol sees the content read-only, with the reason', async () => {
    const carol = await signIn(browser, 'carol');
    await carol.goto(`/items/${code}`);
    await expect(editor(carol).getByText('The server copy.')).toBeVisible();
    const reason = carol.getByTestId('editor-read-only-reason');
    await expect(reason).toContainText('You cannot edit this item');
    await expect(reason).toContainText(`manage_tasks on the board ${BOARD}`);
    await expect(editor(carol).getByText('Edit', { exact: true })).toHaveCount(0);
    await expect(textarea(carol)).toHaveCount(0);
    await expect(carol.getByRole('button', { name: 'Save', exact: true })).toBeDisabled();
    await expect(carol.getByRole('button', { name: 'Archive', exact: true })).toHaveCount(0);
    await expect(carol.getByRole('button', { name: 'Save metadata' })).toBeDisabled();
    await expect(carol.getByTestId('metadata-read-only-reason')).toContainText(
      'You cannot edit this item',
    );
    await carol.context().close();
  });

  const alice = await signIn(browser, 'alice');
  const mine = 'My draft, not saved yet.';

  await test.step('alice sees Archive, and types a draft', async () => {
    await alice.goto(`/items/${code}`);
    await expect(alice.getByRole('button', { name: 'Archive', exact: true })).toBeVisible();
    await expect(alice.getByTestId('editor-read-only-reason')).toHaveCount(0);
    await editor(alice).getByText('Edit', { exact: true }).click();
    await textarea(alice).fill(mine);
  });

  await test.step('a panel action keeps the page mounted, and the draft stays', async () => {
    const boardPanel = alice.locator('.cl-panel', {
      has: alice.locator('.cl-panel__title', { hasText: 'Board' }),
    });
    const laneField = boardPanel.locator('.cl-field', {
      has: alice.locator('.cl-field__label', { hasText: 'Lane' }),
    });
    await laneField.locator('select').selectOption({ label: 'support' });
    await boardPanel.getByRole('button', { name: 'Set lane', exact: true }).click();
    await expect(alice.locator('.cl-toast', { hasText: 'Lane set to support.' })).toBeVisible();
    await expect(textarea(alice)).toHaveValue(mine);
    // KAIROS-T-0618: the editor did not mount again, so it restored nothing.
    await expect(alice.getByTestId('editor-draft-restored')).toHaveCount(0);
  });

  await test.step('a reload restores the draft; Discard draft drops it', async () => {
    // The beforeunload guard asks while the draft is dirty: accept it.
    alice.once('dialog', (dialog) => dialog.accept());
    await alice.reload();
    await expect(textarea(alice)).toHaveValue(mine, { timeout: 30_000 });
    await alice.getByRole('button', { name: 'Discard draft' }).click();
    await expect(textarea(alice)).toHaveValue('The server copy.');
    await alice.reload();
    await expect(alice.getByTestId('editor-draft-restored')).toHaveCount(0, { timeout: 30_000 });
  });

  await test.step('a save removes the stored draft', async () => {
    await editor(alice).getByText('Edit', { exact: true }).click();
    await textarea(alice).fill('Saved by alice.');
    await alice.getByRole('button', { name: 'Save', exact: true }).click();
    await expect(alice.locator('.cl-toast', { hasText: 'Saved' })).toBeVisible();
    await alice.reload();
    await expect(editor(alice).getByText('Saved by alice.')).toBeVisible({ timeout: 30_000 });
    await expect(alice.getByTestId('editor-draft-restored')).toHaveCount(0);
  });

  await alice.context().close();
  // Put the task away: the seeded board stays as it was found.
  await fetch(`${GUI}/api/tasks/${code}`, {
    method: 'DELETE',
    headers: { authorization: `Bearer ${token}` },
  });
});
