// KAIROS-T-0618 — the item page stays mounted on a save and a panel action,
// loads the lists of repositories and boards when their control opens, and
// reads the metadata definitions and values together.
//
// The cast: alice is an org admin, so each control of the page shows.
//
//   1. the page load reads no list of repositories and no list of boards;
//      it reads the metadata definitions and values
//   2. a panel action (Set lane) reads the item once and no panel again;
//      the socket stays open and the draft stays in the editor, with no
//      "Draft restored" notice (the editor did not mount again)
//   3. a save reads nothing: the PATCH gives the item
//   4. the repository control reads the repositories when it opens, and the
//      board move control reads the boards when it opens
//
// The task is new for each run (a timestamped title); the spec archives it
// at the end.

import { test, expect, type Page, type Request } from '@playwright/test';
import { mintToken } from '../helpers/auth';
import { createTask, loadBoard } from '../helpers/api';

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

/** The paths of the GET requests to the API, from the time of the call. */
function recordReads(page: Page): { paths: string[]; reset: () => void } {
  const record = { paths: [] as string[], reset: () => record.paths.splice(0) };
  page.on('request', (request: Request) => {
    const url = new URL(request.url());
    if (request.method() === 'GET' && url.pathname.startsWith('/api/')) {
      record.paths.push(url.pathname);
    }
  });
  return record;
}

/** The reads of a panel; a page that mounts again makes each of them. */
const PANEL_READS = [
  /\/children-progress$/,
  /\/relationships$/,
  /\/metadata$/,
  /^\/api\/metadata-definitions$/,
  /\/external$/,
  /\/proposals$/,
  /^\/api\/boards\/[^/]+$/,
];

const LISTS = [/^\/api\/repositories$/, /^\/api\/boards$/];

const matching = (paths: string[], patterns: RegExp[]) =>
  paths.filter((path) => patterns.some((pattern) => pattern.test(path)));

const editor = (page: Page) => page.locator('.kairos-editor');
const textarea = (page: Page) => page.locator('#kairos-editor-content');
const boardPanel = (page: Page) =>
  page.locator('.cl-panel', { has: page.locator('.cl-panel__title', { hasText: 'Board' }) });

test('the item page stays mounted and loads lists on open', async ({ page }) => {
  const token = await mintToken();
  const board = await loadBoard(GUI, token, (b) => b.slug === BOARD);
  const task = await createTask(GUI, token, {
    title: `Stays mounted e2e ${Date.now()}`,
    boardId: board.id,
    content: 'The server copy.',
  });
  const code: string = task.short_code;

  await login(page);
  const reads = recordReads(page);
  let sockets = 0;
  page.on('websocket', () => {
    sockets += 1;
  });

  await test.step('the page load reads no list of repositories or boards', async () => {
    await page.goto(`/items/${code}`);
    await expect(editor(page).getByText('The server copy.')).toBeVisible();
    await expect(page.locator('[data-testid="repository-control"]')).toBeVisible();
    await expect(page.locator('[data-testid="move-board"]')).toBeVisible();
    await expect(page.getByRole('button', { name: 'Change repository' })).toBeVisible();
    await expect(page.getByRole('button', { name: 'Move to another board' })).toBeVisible();
    expect(matching(reads.paths, LISTS)).toEqual([]);
    expect(reads.paths).toContain('/api/metadata-definitions');
    expect(reads.paths).toContain(`/api/tasks/${code}/metadata`);
    await expect.poll(() => sockets).toBeGreaterThan(0);
  });

  const mine = 'My draft, not saved yet.';
  await test.step('a panel action reads the item once, and no panel', async () => {
    await editor(page).getByText('Edit', { exact: true }).click();
    await textarea(page).fill(mine);
    const socketsBefore = sockets;
    reads.reset();
    const laneField = boardPanel(page).locator('.cl-field', {
      has: page.locator('.cl-field__label', { hasText: 'Lane' }),
    });
    await laneField.locator('select').selectOption({ label: 'support' });
    await boardPanel(page).getByRole('button', { name: 'Set lane', exact: true }).click();
    await expect(page.locator('.cl-toast', { hasText: 'Lane set to support.' })).toBeVisible();
    await expect(page.locator('.cl-pill', { hasText: 'support lane' })).toBeVisible();
    expect(reads.paths.filter((path) => path === `/api/tasks/${code}`)).toHaveLength(1);
    expect(matching(reads.paths, PANEL_READS)).toEqual([]);
    expect(matching(reads.paths, LISTS)).toEqual([]);
    expect(sockets).toBe(socketsBefore);
    await expect(textarea(page)).toHaveValue(mine);
    await expect(page.getByTestId('editor-draft-restored')).toHaveCount(0);
  });

  await test.step('a save reads nothing', async () => {
    reads.reset();
    await page.getByRole('button', { name: 'Save', exact: true }).click();
    await expect(page.locator('.cl-toast', { hasText: 'Saved' })).toBeVisible();
    expect(reads.paths.filter((path) => path === `/api/tasks/${code}`)).toEqual([]);
    expect(matching(reads.paths, PANEL_READS)).toEqual([]);
  });

  await test.step('the controls read their lists when they open', async () => {
    reads.reset();
    await page.getByRole('button', { name: 'Change repository' }).click();
    await expect(page.locator('[data-testid="repository-control"] select')).toBeVisible();
    expect(reads.paths).toContain('/api/repositories');
    await page.getByRole('button', { name: 'Move to another board' }).click();
    await expect(page.locator('[data-testid="move-board"] select')).toBeVisible();
    expect(reads.paths).toContain('/api/boards');
  });

  // Put the task away: the seeded board stays as it was found.
  await fetch(`${GUI}/api/tasks/${code}`, {
    method: 'DELETE',
    headers: { authorization: `Bearer ${token}` },
  });
});
