// KAIROS-T-0586 — the filters of the activity feed (/activity). The team
// filter is in team-lens.spec.ts, and the item picker and the Since field
// in list-pages.spec.ts. This spec proves that each other filter narrows
// the feed, alone and with another one:
//
//   1. Item: the feed has the entries of one task only (its create and its
//      transition);
//   2. Item and Action: the transition only;
//   3. Actor: the entries of bob only; Actor and Item: the create of the
//      task of bob;
//   4. Since a day that has not come: the feed says that no entry agrees,
//      and names Clear;
//   5. Clear empties the form and gives the whole feed again.
//
// The tasks are new for each run; the spec archives them at the end.

import { test, expect, type Page } from '@playwright/test';
import { mintToken } from '../helpers/auth';
import { createTask, loadBoard, transitionTask, tryArchiveTask } from '../helpers/api';

const GUI = process.env.E2E_GUI_BASE_URL ?? 'http://localhost:41080';
const RUN = Date.now().toString(36);

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

/** The rows of the feed as `actor | action | code`. */
async function feed(page: Page): Promise<string[]> {
  return page
    .getByRole('table', { name: 'Activity feed' })
    .locator('tbody tr')
    .evaluateAll((rows) =>
      rows.map((row) => {
        const cells = row.querySelectorAll('td');
        const text = (i: number) => (cells[i]?.textContent ?? '').trim();
        const code = (cells[3]?.querySelector('a')?.textContent ?? '').trim();
        return `${text(1)} | ${text(2)} | ${code}`;
      }),
    );
}

test('activity: the filters Item, Action, Actor and Since, alone and together', async ({
  page,
}) => {
  // Each page load compiles the wasm bundle of the app again.
  test.setTimeout(180_000);
  const alice = await mintToken();
  const bob = await mintToken({ email: 'bob@kairos.test', password: 'bob-password' });
  const board = await loadBoard(GUI, alice, (b) => b.slug === 'platform-delivery');
  const mine = (await createTask(GUI, alice, { title: `E2E activity ${RUN}`, boardId: board.id }))
    .short_code as string;
  const bobs = (await createTask(GUI, bob, { title: `E2E activity bob ${RUN}`, boardId: board.id }))
    .short_code as string;
  await transitionTask(GUI, alice, mine, board.columnId.get('Todo')!);
  const apply = () => page.getByRole('button', { name: 'Apply', exact: true }).click();
  const item = page.getByLabel('Item (code or words)');
  const actor = page.getByLabel('Actor');
  const action = page.getByLabel('Action');

  try {
    await test.step('login, open the activity page', async () => {
      await login(page);
      await page.goto('/activity');
      await expect(page.getByRole('table', { name: 'Activity feed' })).toBeVisible({
        timeout: 30_000,
      });
    });

    await test.step('Item: the entries of one task', async () => {
      await item.fill(mine);
      await apply();
      await expect
        .poll(() => feed(page))
        .toEqual([`alice | transition | ${mine}`, `alice | create | ${mine}`]);
    });

    await test.step('Item and Action: the transition only', async () => {
      await action.selectOption('transition');
      await apply();
      await expect.poll(() => feed(page)).toEqual([`alice | transition | ${mine}`]);
    });

    await test.step('Actor: bob only; Actor and Item: the create of his task', async () => {
      await action.selectOption('(all)');
      await item.fill('');
      await actor.selectOption({ label: 'bob <bob@kairos.test>' });
      await apply();
      // Each row is of bob, and the create of his task is one of them.
      await expect
        .poll(async () => {
          const rows = await feed(page);
          return rows.length > 0 && rows.every((row) => row.startsWith('bob | '))
            ? rows.includes(`bob | create | ${bobs}`)
            : rows;
        })
        .toBe(true);
      await item.fill(bobs);
      await apply();
      await expect.poll(() => feed(page)).toEqual([`bob | create | ${bobs}`]);
      await item.fill(mine);
      await apply();
      await expect(
        page.getByText('No entry agrees with the filters. Change a filter, or select Clear.'),
      ).toBeVisible();
    });

    await test.step('Since a day to come: no entry agrees', async () => {
      await actor.selectOption('(all)');
      const tomorrow = new Date(Date.now() + 2 * 86_400_000).toISOString().slice(0, 10);
      await page.getByLabel('Since (UTC)').fill(tomorrow);
      await apply();
      await expect(
        page.getByText('No entry agrees with the filters. Change a filter, or select Clear.'),
      ).toBeVisible();
    });

    await test.step('Clear empties the form and gives the whole feed', async () => {
      await page.getByRole('button', { name: 'Clear', exact: true }).click();
      await expect(item).toHaveValue('');
      await expect(actor).toHaveValue('(all)');
      await expect(action).toHaveValue('(all)');
      await expect(page.getByLabel('Since (UTC)')).toHaveValue('');
      await expect
        .poll(async () => {
          const rows = await feed(page);
          return rows.includes(`alice | transition | ${mine}`) &&
            rows.includes(`bob | create | ${bobs}`);
        })
        .toBe(true);
    });
  } finally {
    await tryArchiveTask(GUI, alice, mine);
    await tryArchiveTask(GUI, alice, bobs);
  }
});
