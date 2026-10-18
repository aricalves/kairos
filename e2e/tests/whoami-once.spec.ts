// KAIROS-T-0622 — one whoami for each page load, and /teams reads the members of
// every team together.
//
// A reload with a session cookie asks `GET /api/whoami` once: the session probe.
// Its body is the first value of the shell's whoami, and the Admin nav entry and
// the admin pages read that value; none of them asks again. Before KAIROS-T-0622 a
// reload of a board asked four times (the probe, the shell twice, the Admin nav
// entry), and /admin two more.
//
// `/teams` asks for the members of each team. Before KAIROS-T-0622 it waited for
// one answer before it sent the next request: 20 teams were 21 round trips one
// after another. Now each members request goes before the first one answers.
//
// The session is a password session (local-login.spec.ts): the cookie path of the
// restore, the one that has the probe.

import { test, expect, type Page } from '@playwright/test';

const EMAIL = process.env.E2E_LOCAL_EMAIL ?? 'alice@kairos.test';
const PASSWORD = process.env.E2E_LOCAL_PASSWORD ?? 'alice-local-password';

/** The GET requests to /api/whoami, from `start()`. */
function watchWhoami(page: Page) {
  let count = 0;
  page.on('request', (request) => {
    if (request.method() === 'GET' && new URL(request.url()).pathname === '/api/whoami') {
      count += 1;
    }
  });
  return {
    start: () => {
      count = 0;
    },
    count: () => count,
  };
}

async function signIn(page: Page) {
  await page.goto('/login');
  await page.getByLabel('Email').fill(EMAIL);
  await page.getByLabel('Password').fill(PASSWORD);
  await page.getByLabel('Password').press('Enter');
  await expect(page.getByRole('link', { name: 'Boards', exact: true })).toBeVisible();
}

test('whoami: a reload asks once, and the admin pages ask no more', async ({ page }) => {
  await signIn(page);
  const whoami = watchWhoami(page);

  await test.step('a reload of a board asks whoami once', async () => {
    await page.getByRole('link', { name: 'Boards', exact: true }).click();
    await page.getByRole('link', { name: /Platform Delivery/ }).first().click();
    await expect(page.locator('section.kairos-board__column').first()).toBeVisible();
    whoami.start();
    await page.reload();
    await expect(page.locator('section.kairos-board__column').first()).toBeVisible({
      timeout: 15_000,
    });
    // The header and the Admin nav entry both need whoami.
    await expect(page.locator('.cl-appshell__header').getByText('alice')).toBeVisible();
    await expect(page.getByRole('link', { name: 'Admin', exact: true })).toBeVisible();
    await page.waitForLoadState('networkidle');
    expect(whoami.count(), 'the session probe only').toBe(1);
  });

  await test.step('the admin pages read the same whoami', async () => {
    whoami.start();
    await page.getByRole('link', { name: 'Admin', exact: true }).click();
    await expect(page.getByRole('tab', { name: 'Members' })).toBeVisible();
    await page.getByRole('tab', { name: 'Boards' }).click();
    await expect(page).toHaveURL(/\/admin\/boards$/);
    await page.getByRole('tab', { name: 'Members' }).click();
    await expect(page).toHaveURL(/\/admin\/members$/);
    await page.waitForLoadState('networkidle');
    expect(whoami.count(), 'no page under the shell asks whoami').toBe(0);
  });

  await test.step('a reload of an admin page asks whoami once', async () => {
    whoami.start();
    await page.reload();
    await expect(page.getByRole('tab', { name: 'Members' })).toBeVisible({ timeout: 15_000 });
    await page.waitForLoadState('networkidle');
    expect(whoami.count()).toBe(1);
  });
});

test('teams: the members of every team are asked for together', async ({ page }) => {
  await signIn(page);

  // The order of the events: each members request, and each answer.
  const events: ('request' | 'answer')[] = [];
  const isMembers = (url: string) => /^\/api\/teams\/[^/]+\/members$/.test(new URL(url).pathname);
  page.on('request', (request) => {
    if (request.method() === 'GET' && isMembers(request.url())) events.push('request');
  });
  page.on('requestfinished', (request) => {
    if (request.method() === 'GET' && isMembers(request.url())) events.push('answer');
  });

  await page.waitForLoadState('networkidle');
  events.length = 0;
  await page.getByRole('link', { name: 'Teams', exact: true }).click();
  await expect(page.locator('.kairos-board-tile').first()).toBeVisible();
  const tiles = await page.locator('.kairos-board-tile').count();
  expect(tiles, 'the seed has more than one team').toBeGreaterThan(1);
  await page.waitForLoadState('networkidle');

  const requests = events.filter((e) => e === 'request').length;
  expect(requests, 'one members request for each team').toBe(tiles);
  // Every request went before the first answer came.
  expect(events.slice(0, requests)).toEqual(Array(requests).fill('request'));
  // Each tile still shows its count.
  await expect(page.locator('.kairos-board-tile').first()).toContainText(/\d+ members?/);
});
