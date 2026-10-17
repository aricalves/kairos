// KAIROS-T-0615 — a board event patches the card that it names.
//
// Before the task, each event of a board made each open tab read the
// detail of the board and each page of its cards. A note by an agent on
// one card cost every open tab the full board. Now the view reads the one
// card: `GET /api/boards/<slug>/items?code=<short_code>`.
//
//   1. alice makes a task on platform-delivery, through the API
//   2. REAL PKCE login (alice), open the board → the first read has the
//      detail and the cards
//   3. a second writer adds a note to the task → the view reads that card,
//      and it does not read the board again
//   4. a second writer changes the title → the card shows the new title,
//      and the view does not read the board again
//   5. archive the task (also when a step before this one fails)

import { test, expect, type Page } from '@playwright/test';
import { mintToken } from '../helpers/auth';
import { createTask, getTask, patchTask, tryArchiveTask } from '../helpers/api';

const GUI = process.env.E2E_GUI_BASE_URL ?? 'http://localhost:41080';
const RUN = Date.now().toString(36);
const BOARD = 'platform-delivery';

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

/** POST /api/tasks/{code}/notes as `token`. */
async function appendNote(token: string, code: string, text: string): Promise<void> {
  const res = await fetch(`${GUI}/api/tasks/${code}/notes`, {
    method: 'POST',
    headers: { authorization: `Bearer ${token}`, 'content-type': 'application/json' },
    body: JSON.stringify({ text }),
  });
  if (!res.ok) {
    throw new Error(`note on ${code} -> ${res.status}: ${await res.text()}`);
  }
}

test('board view: a note on one card does not refetch the board', async ({ page }) => {
  const alice = await mintToken({ server: GUI, email: 'alice@kairos.test' });
  const title = `live patch ${RUN}`;
  const task = await createTask(GUI, alice, { title, boardId: BOARD });
  const code = task.short_code as string;
  const items = `/api/boards/${BOARD}/items`;

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
      requests.start();
      await page.goto(`/boards/${BOARD}`);
      await expect(page.locator('article.kairos-card', { hasText: title })).toBeVisible({
        timeout: 30_000,
      });
      const paths = requests.reads().map((read) => read.path);
      expect(paths).toContain(`/api/boards/${BOARD}`);
      expect(paths).toContain(items);
    });

    await test.step('a note on the card: the view reads that card only', async () => {
      requests.start();
      const card = page.waitForRequest((request) => {
        const url = new URL(request.url());
        return url.pathname === items && url.searchParams.get('code') === code;
      });
      await appendNote(alice, code, `A note of the e2e run ${RUN}.`);
      await card;
      // The 250 ms window and the read of the card: then no more reads.
      await page.waitForTimeout(1_500);
      const reads = requests.reads();
      expect(
        reads.filter((read) => read.path === items && read.code === null),
        'no read of each card of the board',
      ).toEqual([]);
      expect(
        reads.filter((read) => read.path === `/api/boards/${BOARD}`),
        'no read of the detail of the board',
      ).toEqual([]);
    });

    await test.step('a new title: the card shows it, and the board is not read', async () => {
      requests.start();
      const now = await getTask(GUI, alice, code);
      const renamed = `${title} renamed`;
      await patchTask(GUI, alice, code, {
        title: renamed,
        content: now.content,
        version: now.version,
      });
      await expect(page.locator('article.kairos-card', { hasText: renamed })).toBeVisible({
        timeout: 20_000,
      });
      const reads = requests.reads();
      expect(reads.some((read) => read.path === items && read.code === code)).toBe(true);
      expect(reads.filter((read) => read.path === items && read.code === null)).toEqual([]);
    });
  } finally {
    expect([200, 204]).toContain(await tryArchiveTask(GUI, alice, code));
  }
});
