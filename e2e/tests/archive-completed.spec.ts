// KAIROS-T-0586 — "Archive completed" of a delivery board (KAIROS-T-0363).
//
// The button archives each live task in the done columns of the board, so
// the spec makes a team of its own (and so a delivery board of its own):
// the seeded Completed cards of the demo stay for the other specs.
//
//   1. a board with two completed tasks and one open task: the button is
//      on; the confirmation gives the count and says that each task can be
//      restored;
//   2. Archive: the toast gives the count, the two cards leave the board,
//      the open card stays, and the button is off;
//   3. the server has the two tasks archived, and the open one live;
//   4. bob, who may not archive the tasks of the board, gets no button.
//
// The spec archives the open task and deletes the team at the end.

import { test, expect, type Browser, type Page } from '@playwright/test';
import { mintToken } from '../helpers/auth';
import { createTask, transitionTask, tryArchiveTask } from '../helpers/api';

const GUI = process.env.E2E_GUI_BASE_URL ?? 'http://localhost:41080';
const RUN = Date.now().toString(36);
const TEAM = `e2e-archive-${RUN}`;

async function login(page: Page, email = 'alice@kairos.test', password = 'alice-password') {
  await page.goto('/');
  await page.waitForSelector('#login', { timeout: 30_000 });
  await page.fill('#login', email);
  await page.fill('#password', password);
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

/** Move a task to the first done column, one allowed hop at a time. */
async function complete(token: string, boardId: string, code: string): Promise<void> {
  const board = await call(token, 'GET', `/api/boards/${boardId}`);
  const done = new Set((board.columns as any[]).filter((c) => c.is_done).map((c) => c.id));
  const task = await call(token, 'GET', `/api/tasks/${code}`);
  // A breadth-first search over the transitions of the board.
  const before = new Map<string, string>([[task.column_id, '']]);
  const queue = [task.column_id as string];
  let end: string | undefined;
  while (queue.length > 0 && !end) {
    const at = queue.shift()!;
    for (const t of board.transitions as any[]) {
      if (t.from_column_id !== at || before.has(t.to_column_id)) continue;
      before.set(t.to_column_id, at);
      if (done.has(t.to_column_id)) end = t.to_column_id;
      queue.push(t.to_column_id);
    }
  }
  expect(end, 'a done column the task can reach').toBeTruthy();
  const path: string[] = [];
  for (let at = end!; at !== task.column_id; at = before.get(at)!) path.unshift(at);
  for (const column of path) await transitionTask(GUI, token, code, column);
}

async function asBob(browser: Browser, slug: string): Promise<void> {
  const context = await browser.newContext();
  try {
    const page = await context.newPage();
    await login(page, 'bob@kairos.test', 'bob-password');
    await page.goto(`/boards/${slug}`);
    await expect(page.locator('section.kairos-board__column').first()).toBeVisible({
      timeout: 30_000,
    });
    await expect(page.getByRole('button', { name: 'Flow', exact: true })).toBeVisible();
    await expect(page.getByRole('button', { name: 'Archive completed', exact: true })).toHaveCount(
      0,
    );
  } finally {
    await context.close();
  }
}

test('Archive completed archives the done tasks of the board, after the count', async ({
  page,
  browser,
}) => {
  // Each page load compiles the wasm bundle of the app again.
  test.setTimeout(180_000);
  const token = await mintToken();
  const team = await call(token, 'POST', '/api/teams', {
    name: `Archive ${RUN}`,
    slug: TEAM,
    code_prefix: `AC${RUN.toUpperCase()}`.slice(0, 10),
  });
  const boardId: string = team.delivery_board_id;
  const codes: string[] = [];
  const card = (code: string) => page.locator('article.kairos-card', { hasText: code });
  const button = page.getByRole('button', { name: 'Archive completed', exact: true });

  try {
    const made = async (name: string) => {
      const task = await createTask(GUI, token, { title: `E2E ${name} ${RUN}`, boardId });
      codes.push(task.short_code);
      return task.short_code as string;
    };
    const first = await made('done one');
    const second = await made('done two');
    const open = await made('still open');
    await complete(token, boardId, first);
    await complete(token, boardId, second);
    const board = await call(token, 'GET', `/api/boards/${boardId}`);

    await test.step('login, open the board: the confirmation gives the count', async () => {
      await login(page);
      await page.goto(`/boards/${board.slug}`);
      await expect(card(first)).toBeVisible({ timeout: 30_000 });
      await expect(card(open)).toBeVisible();
      await expect(button).toBeEnabled();
      await button.click();
      const dialog = page.getByRole('dialog');
      await expect(dialog).toContainText('Archive 2 completed tasks?');
      await expect(dialog).toContainText('You can restore each one.');
      await dialog.getByRole('button', { name: 'Archive', exact: true }).click();
    });

    await test.step('the two done cards go, the open card stays, the button is off', async () => {
      await expect(
        page.locator('.cl-toast__text', {
          hasText: 'Kairos archived 2 completed tasks. You can restore each one.',
        }),
      ).toBeVisible();
      await expect(card(first)).toHaveCount(0);
      await expect(card(second)).toHaveCount(0);
      await expect(card(open)).toBeVisible();
      await expect(button).toBeDisabled();
    });

    await test.step('the server agrees', async () => {
      for (const code of [first, second]) {
        const res = await fetch(`${GUI}/api/tasks/${code}`, {
          headers: { authorization: `Bearer ${token}` },
        });
        // An archived task reads with its archive time (ADR-20), or not at all.
        if (res.ok) expect((await res.json()).archived_at, code).toBeTruthy();
        else expect(res.status, code).toBe(404);
      }
      expect((await call(token, 'GET', `/api/tasks/${open}`)).archived_at ?? null).toBeNull();
    });

    await test.step('bob may not archive the tasks of the board: no button', async () => {
      await asBob(browser, board.slug);
    });
  } finally {
    for (const code of codes) await tryArchiveTask(GUI, token, code);
    await fetch(`${GUI}/api/teams/${team.id}`, {
      method: 'DELETE',
      headers: { authorization: `Bearer ${token}` },
    });
  }
});
