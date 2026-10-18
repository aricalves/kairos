// KAIROS-T-0648 — the Reopen button on the item page of a done task:
//
//   1. a task that is not done shows no Reopen button;
//   2. a completed task shows Reopen; the reopen moves it to the entry
//      column of its board;
//   3. a cancelled task shows Reopen; the reopen removes the cancel mark.
//
// Each task is new for each run (a timestamped title), so a retry starts
// clean.

import { test, expect, Page } from '@playwright/test';
import { mintToken } from '../helpers/auth';
import { createTask, loadBoard, readTask, transitionTask } from '../helpers/api';

const GUI = process.env.E2E_GUI_BASE_URL ?? 'http://localhost:41080';

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

async function reopenOnPage(page: Page, code: string): Promise<void> {
  await page.getByRole('button', { name: 'Reopen', exact: true }).click();
  const dialog = page.getByRole('dialog', { name: `Reopen ${code}?` });
  await expect(dialog).toBeVisible();
  await dialog.getByRole('button', { name: 'Reopen', exact: true }).click();
  await expect(
    page.locator('.cl-toast', { hasText: `Kairos reopened ${code}.` }),
  ).toBeVisible();
}

test('a completed or cancelled task is reopened from the item page', async ({ page }) => {
  const token = await mintToken();
  const board = await loadBoard(GUI, token, (b) => b.slug === 'platform-delivery');
  const id = (name: string): string => {
    const column = board.columnId.get(name);
    if (!column) throw new Error(`platform-delivery has no ${name} column`);
    return column;
  };

  const completed = await createTask(GUI, token, {
    title: `E2E reopen done ${Date.now()}`,
    boardId: board.id,
  });
  const cancelled = await createTask(GUI, token, {
    title: `E2E reopen cancelled ${Date.now()}`,
    boardId: board.id,
  });

  await test.step('login via Dex as alice', () => login(page));

  await test.step('a task that is not done shows no Reopen button', async () => {
    await page.goto(`/items/${completed.short_code}`);
    await expect(page.getByRole('button', { name: 'Cancel task', exact: true })).toBeVisible();
    await expect(page.getByRole('button', { name: 'Reopen', exact: true })).toHaveCount(0);
  });

  await test.step('Reopen moves a completed task to the entry column', async () => {
    const code: string = completed.short_code;
    // The task starts in the entry column; the seeded flow goes on from
    // Todo.
    if (completed.column_id !== id('Todo')) {
      await transitionTask(GUI, token, code, id('Todo'));
    }
    for (const column of ['Active', 'Completed']) {
      await transitionTask(GUI, token, code, id(column));
    }
    await page.goto(`/items/${code}`);
    await expect(page.getByRole('button', { name: 'Cancel task', exact: true })).toHaveCount(0);
    await reopenOnPage(page, code);
    await expect(page.getByRole('button', { name: 'Reopen', exact: true })).toHaveCount(0);
    expect((await readTask(GUI, token, code)).column_id).toBe(board.entryColumnId);
  });

  await test.step('Reopen removes the cancel mark of a cancelled task', async () => {
    const code: string = cancelled.short_code;
    const res = await fetch(`${GUI}/api/tasks/${code}/cancel`, {
      method: 'POST',
      headers: { authorization: `Bearer ${token}`, 'content-type': 'application/json' },
      body: JSON.stringify({ reason: 'The team does not need it.' }),
    });
    expect(res.ok).toBeTruthy();
    await page.goto(`/items/${code}`);
    const banner = page.locator('[data-testid="cancelled-banner"]');
    await expect(banner).toBeVisible();
    await reopenOnPage(page, code);
    await expect(banner).toHaveCount(0);
    const task = await readTask(GUI, token, code);
    expect(task.cancellation ?? null).toBeNull();
    expect(task.column_id).toBe(board.entryColumnId);
  });
});
