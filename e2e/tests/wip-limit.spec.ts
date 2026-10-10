// KAIROS-T-0371 — the WIP limit of a column in the GUI:
//
//   1. the board settings page sets the WIP limit of Active to 1;
//   2. with 2 or more items in Active, the board page shows the chip
//      "WIP n/1 over" on the column head;
//   3. the settings page removes the limit (an empty field), and the chip
//      goes.
//
// The spec makes its own 2 tasks in Active and archives them after.

import { test, expect, type Page } from '@playwright/test';
import { mintToken } from '../helpers/auth';
import { createTask, loadBoard, transitionTask, tryArchiveTask } from '../helpers/api';

const GUI = process.env.E2E_GUI_BASE_URL ?? 'http://localhost:41080';

async function login(page: Page): Promise<void> {
  await page.goto('/');
  await page.waitForSelector('#login', { timeout: 30_000 });
  await page.fill('#login', 'alice@kairos.test');
  await page.fill('#password', 'alice-password');
  await page.click('#submit-login');
  await page.waitForURL((url) => url.pathname.startsWith('/boards'), { timeout: 30_000 });
}

test('a column has a WIP limit, and the board marks the column over it', async ({ page }) => {
  const token = await mintToken();
  const board = await loadBoard(GUI, token, (b) => b.slug === 'platform-delivery');
  const todo = board.columnId.get('Todo')!;
  const active = board.columnId.get('Active')!;
  const codes: string[] = [];
  for (const n of [1, 2]) {
    const task = await createTask(GUI, token, { title: `E2E WIP ${n} ${Date.now()}`, boardId: board.id });
    if (task.column_id !== todo) await transitionTask(GUI, token, task.short_code, todo);
    await transitionTask(GUI, token, task.short_code, active);
    codes.push(task.short_code);
  }
  const activeRow = () => page.locator('[data-testid="column-row"][data-column="Active"]');

  try {
    await test.step('login, and set the limit of Active to 1', async () => {
      await login(page);
      await page.goto(`/admin/boards/${board.id}`);
      const field = activeRow().locator('[data-testid="wip-limit"] input');
      await expect(field).toBeVisible({ timeout: 30_000 });
      await field.fill('1');
      await activeRow().getByRole('button', { name: 'Set limit' }).click();
      await expect(page.getByText('Kairos set the WIP limit of the column to 1.')).toBeVisible();
    });

    await test.step('the board marks Active over its limit', async () => {
      await page.goto('/boards/platform-delivery');
      const chip = page
        .locator('section.kairos-board__column', { hasText: 'Active' })
        .first()
        .locator('[data-testid="wip"]');
      await expect(chip).toContainText(/WIP \d+\/1 over/);
      await expect(chip).toHaveClass(/kairos-board__wip--over/);
    });

    await test.step('an empty field removes the limit', async () => {
      await page.goto(`/admin/boards/${board.id}`);
      const field = activeRow().locator('[data-testid="wip-limit"] input');
      await expect(field).toHaveValue('1', { timeout: 30_000 });
      await field.fill('');
      await activeRow().getByRole('button', { name: 'Set limit' }).click();
      await expect(page.getByText('Kairos removed the WIP limit of the column.')).toBeVisible();
      await page.goto('/boards/platform-delivery');
      await expect(page.locator('article.kairos-card').first()).toBeVisible();
      await expect(page.locator('[data-testid="wip"]')).toHaveCount(0);
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
