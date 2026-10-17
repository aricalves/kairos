// KAIROS-T-0619 — a drop moves the card at once, and the echo patches it.
//
// Before the task, a drop sent the transition and then read the whole
// board. The card stayed in its old column for a POST and two GETs. Now
// the card moves in the view at once. The WebSocket echo of the move reads
// that one card (`GET /api/boards/<slug>/items?code=<short_code>`).
//
//   1. alice makes a task on platform-delivery, through the API (Backlog)
//   2. REAL PKCE login (alice), open the board
//   3. drag the card to Todo, and hold the POST of the transition → the
//      card is in Todo before the server answers; then the view reads that
//      card only, and it does not read the board again
//   4. drag the card to Active, and the stubbed server refuses the move →
//      the card goes back to Todo, and the toast shows the message of the
//      server
//   5. archive the task (also when a step before this one fails)

import { test, expect, type Locator, type Page } from '@playwright/test';
import { mintToken } from '../helpers/auth';
import { createTask, tryArchiveTask } from '../helpers/api';
import { dragTo } from '../helpers/drag';

const GUI = process.env.E2E_GUI_BASE_URL ?? 'http://localhost:41080';
const RUN = Date.now().toString(36);
const BOARD = 'platform-delivery';
const REFUSAL = 'The board refused this move in the e2e run.';

const column = (page: Page, name: string): Locator =>
  page
    .locator('section.kairos-board__lane--planned')
    .locator('section.kairos-board__column', {
      has: page.locator('.kairos-board__column-head', { hasText: name }),
    });

const cardIn = (page: Page, columnName: string, title: string): Locator =>
  column(page, columnName).locator('article.kairos-card', { hasText: title });

/** The GET requests to the board routes, from `start()`: path and query. */
function watchBoardRequests(page: Page) {
  let reads: { path: string; code: string | null }[] = [];
  page.on('request', (request) => {
    const url = new URL(request.url());
    if (request.method() === 'GET' && url.pathname.startsWith('/api/boards')) {
      reads.push({ path: url.pathname, code: url.searchParams.get('code') });
    }
  });
  return {
    start: () => {
      reads = [];
    },
    reads: () => [...reads],
  };
}

test('board view: a drop moves the card at once and a refusal moves it back', async ({ page }) => {
  const alice = await mintToken({ server: GUI, email: 'alice@kairos.test' });
  const title = `optimistic drop ${RUN}`;
  const task = await createTask(GUI, alice, { title, boardId: BOARD });
  const code = task.short_code as string;
  const items = `/api/boards/${BOARD}/items`;
  const transition = `**/api/tasks/${code}/transition`;

  try {
    const requests = watchBoardRequests(page);

    await test.step('login via Dex as alice, open the board', async () => {
      await page.goto('/');
      await page.waitForSelector('#login', { timeout: 30_000 });
      await page.fill('#login', 'alice@kairos.test');
      await page.fill('#password', 'alice-password');
      await page.click('#submit-login');
      await page.waitForURL((url) => url.pathname === '/' || url.pathname.startsWith('/boards'), {
        timeout: 30_000,
      });
      await page.goto(`/boards/${BOARD}`);
      await expect(cardIn(page, 'Backlog', title)).toBeVisible({ timeout: 30_000 });
    });

    await test.step('drag to Todo: the card moves before the server answers', async () => {
      let release: () => void = () => {};
      const held = new Promise<void>((resolve) => {
        release = resolve;
      });
      await page.route(transition, async (route) => {
        await held;
        await route.continue();
      });
      requests.start();
      const echo = page.waitForRequest((request) => {
        const url = new URL(request.url());
        return url.pathname === items && url.searchParams.get('code') === code;
      });
      await dragTo(page, cardIn(page, 'Backlog', title), column(page, 'Todo'));
      // The POST is held: the card is in Todo all the same.
      await expect(cardIn(page, 'Todo', title)).toBeVisible({ timeout: 5_000 });
      await expect(cardIn(page, 'Backlog', title)).toHaveCount(0);
      release();
      await echo;
      // The 250 ms window and the read of the card: then no more reads.
      await page.waitForTimeout(1_500);
      await page.unroute(transition);
      const reads = requests.reads();
      expect(
        reads.filter((read) => read.path === items && read.code === null),
        'no read of each card of the board',
      ).toEqual([]);
      expect(
        reads.filter((read) => read.path === `/api/boards/${BOARD}`),
        'no read of the detail of the board',
      ).toEqual([]);
      await expect(cardIn(page, 'Todo', title)).toBeVisible();
    });

    await test.step('drag to Active, the server refuses: the card goes back', async () => {
      await page.route(transition, (route) =>
        route.fulfill({
          status: 422,
          contentType: 'application/json',
          body: JSON.stringify({ error: { code: 'INVALID_TRANSITION', message: REFUSAL } }),
        }),
      );
      await dragTo(page, cardIn(page, 'Todo', title), column(page, 'Active'));
      await expect(page.locator('.cl-toast', { hasText: REFUSAL })).toBeVisible({
        timeout: 10_000,
      });
      await expect(cardIn(page, 'Todo', title)).toBeVisible();
      await expect(cardIn(page, 'Active', title)).toHaveCount(0);
      await page.unroute(transition);
    });
  } finally {
    expect([200, 204]).toContain(await tryArchiveTask(GUI, alice, code));
  }
});
