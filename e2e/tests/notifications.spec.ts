// KAIROS-T-0383 — the bell of the shell:
//
//   1. a hand-off to alice shows in her bell with no reload (the count
//      comes over /ws/events);
//   2. the drawer lists the notification, with a link to the task;
//   3. Mark read clears it: the count goes down, and the server has it
//      read (so it is read for her agent too);
//   4. the Notifications page has the settings of the kinds;
//   5. Mark all read on the page clears the bell (KAIROS-T-0387).
//
// The task is new for each run (a timestamped title), so a retry starts
// clean.

import { test, expect, Page } from '@playwright/test';
import { mintToken } from '../helpers/auth';
import { createTask, loadBoard } from '../helpers/api';

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

async function post(token: string, path: string, body: unknown): Promise<any> {
  const res = await fetch(`${GUI}${path}`, {
    method: 'POST',
    headers: { authorization: `Bearer ${token}`, 'content-type': 'application/json' },
    body: JSON.stringify(body),
  });
  if (!res.ok) throw new Error(`POST ${path} -> ${res.status}: ${await res.text()}`);
  return res.json();
}

async function notifications(token: string): Promise<any> {
  const res = await fetch(`${GUI}/api/me/notifications`, {
    headers: { authorization: `Bearer ${token}` },
  });
  if (!res.ok) throw new Error(`notifications -> ${res.status}`);
  return res.json();
}

/** The count of the bell as the page shows it: '' when there is none. */
function countText(unread: number): string {
  if (unread <= 0) return '';
  return unread > 99 ? '99+' : String(unread);
}

test('a hand-off to alice shows in her bell, and Mark read clears it', async ({ page }) => {
  const alice = await mintToken();
  const bob = await mintToken({ email: 'bob@kairos.test', password: 'bob-password' });
  const board = await loadBoard(GUI, alice, (b) => b.slug === 'platform-delivery');
  const todo = board.columnId.get('Todo');
  const active = board.columnId.get('Active');
  if (!todo || !active) throw new Error('platform-delivery has no Todo or Active column');
  const created = await createTask(GUI, alice, {
    title: `E2E bell ${Date.now()}`,
    boardId: board.id,
  });
  const code: string = created.short_code;
  if (created.column_id !== todo) {
    await post(alice, `/api/tasks/${code}/transition`, { to_column_id: todo });
  }
  await post(alice, `/api/tasks/${code}/transition`, { to_column_id: active });
  // alice has the claim; she gives it to bob. bob is told, not alice.
  await post(alice, `/api/tasks/${code}/hand-off`, { to: 'bob@kairos.test' });

  await test.step('login via Dex as alice', () => login(page));
  const bell = page.locator('[data-testid="notification-bell"]');
  await expect(bell).toBeVisible();
  const before = (await notifications(alice)).unread as number;
  const count = page.locator('[data-testid="notification-count"]');
  if (before > 0) {
    await expect(count).toHaveText(countText(before));
  } else {
    await expect(count).toHaveCount(0);
  }

  await test.step('bob hands the task back: the bell counts it, live', async () => {
    await post(bob, `/api/tasks/${code}/hand-off`, { to: 'alice@kairos.test' });
    await expect(count).toHaveText(countText(before + 1), { timeout: 15_000 });
  });

  const row = page.locator('[data-testid="notification"]', { hasText: code });

  await test.step('the drawer lists it, with a link to the task', async () => {
    await bell.getByRole('button', { name: 'Notifications' }).click();
    const drawer = page.locator('[data-testid="notification-drawer"]');
    await expect(drawer).toBeVisible();
    await expect(row.first()).toBeVisible();
    await expect(row.first()).toContainText(`bob gave you the claim on ${code}`);
    await expect(row.first()).toHaveAttribute('data-unread', 'true');
    await expect(row.first().locator(`a[href="/items/${code}"]`)).toBeVisible();
  });

  await test.step('Mark read clears it, for the agent too', async () => {
    await row.first().getByRole('button', { name: 'Mark read', exact: true }).click();
    await expect(row.first()).toHaveAttribute('data-unread', 'false');
    if (before > 0) {
      await expect(count).toHaveText(countText(before));
    } else {
      await expect(count).toHaveCount(0);
    }
    const list = await notifications(alice);
    const mine = (list.items as any[]).filter(
      (n) => n.short_code === code && n.kind === 'hand_off',
    );
    expect(mine).toHaveLength(1);
    expect(mine[0].read_at).toBeTruthy();
    expect(list.unread).toBe(before);
  });

  await test.step('the Notifications page has the settings', async () => {
    await page.goto('/notifications');
    const settings = page.locator('[data-testid="notification-settings"]');
    await expect(settings).toBeVisible();
    await expect(settings).toContainText('A person hands off a task to you');
  });

  await test.step('Mark all read on the page clears the bell', async () => {
    // alice has the claim again: she gives it to bob, and he gives it back,
    // so that she has an unread notification.
    await post(alice, `/api/tasks/${code}/hand-off`, { to: 'bob@kairos.test' });
    await post(bob, `/api/tasks/${code}/hand-off`, { to: 'alice@kairos.test' });
    await expect(count).toHaveCount(1, { timeout: 15_000 });
    await page
      .locator('.cl-panel', { hasText: 'Your notifications' })
      .getByRole('button', { name: 'Mark all read', exact: true })
      .click();
    await expect(
      page.getByText('Kairos marked each notification of the list read.'),
    ).toBeVisible();
    await expect(count).toHaveCount(0);
    expect((await notifications(alice)).unread).toBe(0);
  });
});
