// KAIROS-T-0621 — one WebSocket for each tab.
//
// The bell and each live view share one socket. The socket sends one
// filter for all of them:
//
//   1. the board page: one socket, the filter of the board
//   2. a link to an item: the same socket, the filter of the whole stream
//   3. the notifications page: one socket, the filter `person_only`

import { test, expect, type Page, type WebSocket } from '@playwright/test';

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

/** The sockets of the page, and the subscribe messages that they send. */
function watchSockets(page: Page) {
  let sockets: WebSocket[] = [];
  let filters: any[] = [];
  page.on('websocket', (socket) => {
    if (!socket.url().includes('/ws/events')) {
      return;
    }
    sockets.push(socket);
    socket.on('framesent', (frame) => {
      const message = JSON.parse(String(frame.payload));
      if (message.subscribe !== undefined) {
        filters.push(message.subscribe);
      }
    });
  });
  return {
    start: () => {
      sockets = [];
      filters = [];
    },
    open: () => sockets.filter((socket) => !socket.isClosed()).length,
    count: () => sockets.length,
    last: () => filters[filters.length - 1],
  };
}

test('one socket for each tab, with one filter', async ({ page }) => {
  const sockets = watchSockets(page);
  await login(page);
  // The landing page is For me (KAIROS-T-0490), and it opens its socket
  // after the login. The count of the board page starts after that.
  await expect(page.locator('[data-testid="notification-bell"]')).toBeVisible({
    timeout: 30_000,
  });
  await expect.poll(() => sockets.open()).toBe(1);

  await test.step('the board page: one socket, the filter of the board', async () => {
    sockets.start();
    await page.goto(`/boards/${BOARD}`);
    await expect(page.locator('article.kairos-card').first()).toBeVisible({ timeout: 30_000 });
    await expect(page.locator('[data-testid="notification-bell"]')).toBeVisible();
    await expect.poll(() => sockets.last()?.board_id ?? null).not.toBeNull();
    expect(sockets.count()).toBe(1);
    expect(sockets.open()).toBe(1);
  });

  await test.step('an item page: the same socket, the whole stream', async () => {
    await page.locator('article.kairos-card').first().locator('a.kairos-card__code').click();
    await page.waitForURL(/\/items\//);
    await expect.poll(() => JSON.stringify(sockets.last())).toBe('{}');
    expect(sockets.count()).toBe(1);
    expect(sockets.open()).toBe(1);
  });

  await test.step('the notifications page: one socket, person_only', async () => {
    sockets.start();
    await page.goto('/notifications');
    await expect.poll(() => sockets.last()?.person_only ?? false).toBe(true);
    expect(sockets.count()).toBe(1);
  });
});
