// KAIROS-T-0491 — the lanes of a grouped board collapse.
//
// Read-only on the seeded boards (alice):
//
//   1. REAL PKCE login
//   2. Platform Delivery: collapse the Planned lane. Its head toggle says
//      aria-expanded=false, and the lane is one row: the card count of the
//      lane in each column, the same as the cards in the expanded lane
//   3. reload: the lane stays collapsed (localStorage of this browser)
//   4. Web Delivery is not affected
//   5. "Expand all" and "Collapse all" work on each lane on screen
//   6. the owner grouping keeps its own set: its lanes start expanded
//
// Each Playwright test has a new browser context, so no other spec sees
// the state that this spec stores.

import { test, expect, type Locator, type Page } from '@playwright/test';

const lane = (page: Page, id: string): Locator =>
  page.locator(`section.kairos-board__lane[data-lane-id="${id}"]`);
const toggle = (page: Page, id: string): Locator =>
  lane(page, id).locator('.kairos-board__lane-head [data-testid="lane-toggle"]');

/** The counts of the row of a collapsed lane, by column name. */
async function summaryByColumn(page: Page, id: string): Promise<Map<string, number>> {
  const counts = new Map<string, number>();
  const cells = lane(page, id).locator('.kairos-board__summary-cell');
  for (const cell of await cells.all()) {
    const name = (await cell.getAttribute('data-column')) ?? '';
    const count = Number(await cell.locator('[data-testid="lane-summary-count"]').textContent());
    counts.set(name, count);
  }
  return counts;
}

async function openBoard(page: Page, name: string, slug: string) {
  await page.locator('.cl-appshell__navbar').getByRole('link', { name: 'Boards', exact: true }).click();
  await page.locator('.kairos-board-tile', { hasText: name }).click();
  await page.waitForURL(new RegExp(`/boards/${slug}`));
  await expect(page.locator('section.kairos-board__lane').first()).toBeVisible({ timeout: 20_000 });
}

test('lanes collapse: one row with counts, kept per board and grouping', async ({ page }) => {
  await test.step('login via Dex as alice', async () => {
    await page.goto('/');
    await page.waitForSelector('#login', { timeout: 30_000 });
    await page.fill('#login', 'alice@kairos.test');
    await page.fill('#password', 'alice-password');
    await page.click('#submit-login');
    await page.waitForURL((url) => url.pathname === '/' || url.pathname.startsWith('/boards'), {
      timeout: 30_000,
    });
  });

  let expanded = new Map<string, number>();
  await test.step('collapse the Planned lane: one row, the same counts', async () => {
    await openBoard(page, 'Platform Delivery', 'platform-delivery');
    await expect(toggle(page, 'planned')).toHaveAttribute('aria-expanded', 'true');
    // Read the column names from the cells of the collapsed row, and the
    // cards from the expanded lane: the two must agree.
    const columns = lane(page, 'planned').locator('section.kairos-board__column');
    const names: string[] = [];
    for (const column of await columns.all()) {
      names.push(((await column.locator('.kairos-board__column-head').textContent()) ?? '').trim());
    }
    const cards: number[] = [];
    for (const column of await columns.all()) {
      cards.push(await column.locator('article.kairos-card').count());
    }
    expect(cards.reduce((a, b) => a + b, 0)).toBeGreaterThan(0);

    await toggle(page, 'planned').click();
    await expect(toggle(page, 'planned')).toHaveAttribute('aria-expanded', 'false');
    await expect(lane(page, 'planned')).toHaveClass(/kairos-board__lane--collapsed/);
    await expect(lane(page, 'planned').locator('article.kairos-card')).toHaveCount(0);
    await expect(lane(page, 'planned').locator('[data-testid="lane-summary"]')).toBeVisible();
    expanded = await summaryByColumn(page, 'planned');
    expect([...expanded.values()]).toEqual(cards);
    // Each column head starts with the name of its cell.
    const cellNames = [...expanded.keys()];
    cellNames.forEach((name, i) => expect(names[i].startsWith(name)).toBe(true));
    // The Support lane stays open.
    await expect(toggle(page, 'support')).toHaveAttribute('aria-expanded', 'true');
  });

  await test.step('reload: the lane stays collapsed', async () => {
    await page.reload();
    await expect(lane(page, 'planned')).toHaveClass(/kairos-board__lane--collapsed/, {
      timeout: 20_000,
    });
    await expect(toggle(page, 'planned')).toHaveAttribute('aria-expanded', 'false');
    expect(await summaryByColumn(page, 'planned')).toEqual(expanded);
  });

  await test.step('another board is not affected', async () => {
    await openBoard(page, 'Web Delivery', 'web-delivery');
    await expect(toggle(page, 'planned')).toHaveAttribute('aria-expanded', 'true');
    await expect(lane(page, 'planned')).not.toHaveClass(/kairos-board__lane--collapsed/);
  });

  await test.step('Expand all and Collapse all', async () => {
    await openBoard(page, 'Platform Delivery', 'platform-delivery');
    await expect(toggle(page, 'planned')).toHaveAttribute('aria-expanded', 'false');
    await page.getByTestId('expand-all-lanes').click();
    await expect(page.locator('section.kairos-board__lane--collapsed')).toHaveCount(0);
    await expect(lane(page, "planned").locator("article.kairos-card").first()).toBeVisible();
    await page.getByTestId('collapse-all-lanes').click();
    await expect(page.locator('section.kairos-board__lane--collapsed')).toHaveCount(2);
    // The keyboard works the toggle: Enter on the focused button.
    await toggle(page, 'support').focus();
    await page.keyboard.press('Enter');
    await expect(toggle(page, 'support')).toHaveAttribute('aria-expanded', 'true');
  });

  await test.step('the owner grouping has its own set', async () => {
    await page.getByTestId('group-by-owner').click();
    await page.waitForURL(/by_owner=1/);
    const owners = page.locator('section.kairos-board__lane--owner');
    await expect(owners.first()).toBeVisible();
    await expect(page.locator('section.kairos-board__lane--collapsed')).toHaveCount(0);
    const first = (await owners.first().getAttribute('data-lane-id')) ?? '';
    await toggle(page, first).click();
    await expect(lane(page, first)).toHaveClass(/kairos-board__lane--collapsed/);
    // Back to the level lanes: their set is as it was (Planned collapsed).
    await page.getByTestId('group-by-owner').click();
    await expect(lane(page, 'planned')).toHaveClass(/kairos-board__lane--collapsed/);
    await expect(lane(page, 'support')).not.toHaveClass(/kairos-board__lane--collapsed/);
  });
});
