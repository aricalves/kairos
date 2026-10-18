// KAIROS-T-0582 — the done columns, the card filters, the WIP chip and a
// refused drop on the board page:
//
//   1. a done column with more than 20 cards shows the last 20, and "Show
//      all N cards" shows each card;
//   2. a card shows its priority metadata;
//   3. "Hide done" hides the done columns, and "Mine" keeps the cards of
//      the reader;
//   4. the WIP chip of a column shows once, in the first lane;
//   5. during a drag, a column that the board does not allow shows "No
//      transition from X."
//
// The spec makes its own tasks and archives them after.

import { test, expect, type Page } from '@playwright/test';
import { mintToken } from '../helpers/auth';
import { createTask, loadBoard, transitionTask, tryArchiveTask } from '../helpers/api';

const GUI = process.env.E2E_GUI_BASE_URL ?? 'http://localhost:41080';
const BOARD = 'platform-delivery';
const RUN = Date.now();

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

async function setPriority(token: string, code: string, value: string): Promise<void> {
  const res = await fetch(`${GUI}/api/tasks/${code}/metadata`, {
    method: 'PATCH',
    headers: { authorization: `Bearer ${token}`, 'content-type': 'application/json' },
    body: JSON.stringify({ values: { priority: value } }),
  });
  if (!res.ok) throw new Error(`metadata ${code} -> ${res.status}: ${await res.text()}`);
}

test('board: done columns, priority, card filters, one WIP chip, refused drops', async ({ page }) => {
  test.setTimeout(180_000);
  const token = await mintToken({ server: GUI, email: 'alice@kairos.test' });
  const board = await loadBoard(GUI, token, (b) => b.slug === BOARD);
  const todo = board.columnId.get('Todo')!;
  const active = board.columnId.get('Active')!;
  const completed = board.columnId.get('Completed')!;
  const codes: string[] = [];
  const column = (name: string) =>
    page.locator('section.kairos-board__column', {
      has: page.locator('.kairos-board__column-head', { hasText: name }),
    });

  try {
    await test.step('make 21 completed tasks and 1 task in Todo with a priority', async () => {
      for (let n = 0; n < 21; n += 1) {
        const task = await createTask(GUI, token, { title: `E2E done ${RUN} ${n}`, boardId: board.id });
        if (task.column_id !== todo) await transitionTask(GUI, token, task.short_code, todo);
        await transitionTask(GUI, token, task.short_code, active);
        await transitionTask(GUI, token, task.short_code, completed);
        codes.push(task.short_code);
      }
      const open = await createTask(GUI, token, { title: `E2E open ${RUN}`, boardId: board.id });
      if (open.column_id !== todo) await transitionTask(GUI, token, open.short_code, todo);
      await setPriority(token, open.short_code, 'high');
      codes.push(open.short_code);
    });

    await test.step('the done column shows the last 20 cards, then each card', async () => {
      await login(page);
      await page.goto(`/boards/${BOARD}`);
      const done = column('Completed').last();
      await expect(done.locator('[data-testid="show-all-done"]')).toContainText(/Show all \d+ cards/, {
        timeout: 30_000,
      });
      await expect(done.locator('article.kairos-card')).toHaveCount(20);
      // The first task of the run has the lowest code: it is hidden.
      await expect(done.locator('article.kairos-card', { hasText: `E2E done ${RUN} 0` })).toHaveCount(0);
      await done.locator('[data-testid="show-all-done"]').click();
      await expect(done.locator('article.kairos-card', { hasText: `E2E done ${RUN} 0` })).toHaveCount(1);
      await expect(done.locator('[data-testid="show-all-done"]')).toHaveText('Show the last 20 cards');
    });

    await test.step('the card shows its priority', async () => {
      const card = page.locator('article.kairos-card', { hasText: `E2E open ${RUN}` });
      await expect(card).toContainText('priority high');
    });

    await test.step('"Hide done" hides the done columns; "Mine" keeps my cards', async () => {
      await page.locator('[data-testid="filter-hide-done"]').click();
      await expect(page).toHaveURL(/hide_done=1/);
      await expect(column('Completed')).toHaveCount(0);
      await page.locator('[data-testid="filter-hide-done"]').click();
      await expect(column('Completed').first()).toBeVisible();

      await page.locator('[data-testid="filter-mine"]').click();
      await expect(page).toHaveURL(/mine=1/);
      // The task in Todo has no claim: "Mine" hides it.
      await expect(page.locator('article.kairos-card', { hasText: `E2E open ${RUN}` })).toHaveCount(0);
      await page.locator('[data-testid="filter-mine"]').click();
      await expect(page.locator('article.kairos-card', { hasText: `E2E open ${RUN}` })).toHaveCount(1);
    });

    await test.step('a WIP chip shows once for each column', async () => {
      const res = await fetch(`${GUI}/api/boards/${board.id}/columns/${active}`, {
        method: 'PATCH',
        headers: { authorization: `Bearer ${token}`, 'content-type': 'application/json' },
        body: JSON.stringify({ wip_limit: 5 }),
      });
      expect(res.ok).toBe(true);
      await page.reload();
      await expect(page.locator('article.kairos-card').first()).toBeVisible({ timeout: 30_000 });
      // Two lanes (Support and Planned), one chip.
      await expect(column('Active')).toHaveCount(2);
      await expect(page.locator('[data-testid="wip"]')).toHaveCount(1);
    });

    await test.step('a refused column says why during a drag', async () => {
      const card = page.locator('article.kairos-card', { hasText: `E2E open ${RUN}` });
      // Todo -> Completed is not a transition of the seeded board.
      await card.dispatchEvent('dragstart', { dataTransfer: await page.evaluateHandle(() => new DataTransfer()) });
      await expect(
        column('Completed').locator('[data-testid="no-transition"]').first(),
      ).toHaveText('No transition from Todo.');
      await card.dispatchEvent('dragend');
      await expect(page.locator('[data-testid="no-transition"]')).toHaveCount(0);
    });
  } finally {
    await fetch(`${GUI}/api/boards/${board.id}/columns/${active}`, {
      method: 'PATCH',
      headers: { authorization: `Bearer ${token}`, 'content-type': 'application/json' },
      body: JSON.stringify({ wip_limit: null }),
    });
    for (const code of codes) await tryArchiveTask(GUI, token, code);
  }
});
