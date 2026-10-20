// KAIROS-T-0670 — Admin → Repositories: the interval of the issue import.
//
//   1. An org admin sets the interval of a repository, sees the state (the
//      last complete run, the next run and the counts), changes the interval
//      and turns it off, with no CLI;
//   2. a refusal of the server shows its message next to the select (a
//      repository with the forge `other` has no issues to import);
//   3. a person who is not an org admin sees the interval and the state, and
//      cannot change them.
//
// Each run uses its own repository names (a timestamp). The test puts back
// what it changed, also when it fails.

import { test, expect, type Page } from '@playwright/test';
import { mintToken } from '../helpers/auth';
import { createRepository, readEachPage } from '../helpers/api';

const GUI = process.env.E2E_GUI_BASE_URL ?? 'http://localhost:41080';
const RUN = Date.now().toString(36);

async function login(page: Page, email: string, password: string): Promise<void> {
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

const row = (page: Page, slug: string) => page.locator(`[data-repo="${slug}"]`).first();
const cadence = (page: Page, slug: string) =>
  row(page, slug).locator('[data-testid="issue-cadence"]');
const select = (page: Page, slug: string) => cadence(page, slug).locator('select');
const state = (page: Page, slug: string) =>
  cadence(page, slug).locator('[data-testid="issue-cadence-state"]');

test('issue cadence: an org admin sets, reads and turns off the interval; a refusal shows', async ({
  page,
}) => {
  // Each page load compiles the wasm bundle of the app again.
  test.setTimeout(180_000);
  const token = await mintToken();
  const github = `cad-${RUN}`;
  const other = `cad-other-${RUN}`;
  await createRepository(GUI, token, {
    slug: github,
    repoFullName: `acme/${github}`,
    team: 'platform',
  });
  await createRepository(GUI, token, {
    slug: other,
    forge: 'other',
    repoFullName: `acme/${other}`,
    team: 'platform',
  });

  await test.step('login, open Repositories: the import is off', async () => {
    await login(page, 'alice@kairos.test', 'alice-password');
    await page.goto('/admin/repositories');
    await expect(row(page, github)).toBeVisible({ timeout: 30_000 });
    await expect(select(page, github)).toHaveValue('off');
    await expect(select(page, github)).toBeEnabled();
    await expect(state(page, github)).toContainText('Last complete run: none yet.');
    await expect(state(page, github)).toContainText('Next run: none. The import is off.');
    await expect(state(page, github)).toContainText(
      'Issues in the record: 0. Last run: 0 listed, 0 new tasks, no run yet.',
    );
    await expect(cadence(page, github).getByText('Only an organization admin')).toHaveCount(0);
    // The manual import needs no webhook connection: the button shows for a
    // repository with no connection.
    await expect(row(page, github).getByRole('button', { name: 'Connect webhook' })).toBeVisible();
    await expect(row(page, github).getByRole('button', { name: 'Import open issues' })).toBeEnabled();
  });

  await test.step('set 15 minutes: the page shows the next run', async () => {
    await select(page, github).selectOption('15m');
    await cadence(page, github).getByRole('button', { name: 'Save interval' }).click();
    await expect(
      page.getByText(`Kairos set the import interval of "${github}" to 15m.`),
    ).toBeVisible({ timeout: 10_000 });
    await expect(select(page, github)).toHaveValue('15m');
    await expect(state(page, github)).toContainText(/Next run: 20\d\d-\d\d-\d\dT/);
    expect((await call(token, 'GET', `/api/repositories/${github}`)).issue_import_interval).toBe(
      '15m',
    );
  });

  await test.step('change to one hour, then turn it off', async () => {
    await select(page, github).selectOption('1h');
    await cadence(page, github).getByRole('button', { name: 'Save interval' }).click();
    await expect(
      page.getByText(`Kairos set the import interval of "${github}" to 1h.`),
    ).toBeVisible({ timeout: 10_000 });
    await expect(select(page, github)).toHaveValue('1h');
    // The list reads again after the save above and builds the row anew, so
    // try the choice again if that happens between the choice and the click.
    await expect(async () => {
      await select(page, github).selectOption('off');
      await cadence(page, github).getByRole('button', { name: 'Save interval' }).click();
      await expect(
        page.getByText(`Kairos set the import interval of "${github}" to off.`),
      ).toBeVisible({ timeout: 3_000 });
    }).toPass({ timeout: 20_000 });
    await expect(state(page, github)).toContainText('Next run: none. The import is off.');
    expect((await call(token, 'GET', `/api/repositories/${github}`)).issue_import_interval).toBe(
      'off',
    );
  });

  await test.step('a refusal of the server shows next to the select', async () => {
    await select(page, other).selectOption('15m');
    await cadence(page, other).getByRole('button', { name: 'Save interval' }).click();
    await expect(cadence(page, other).locator('.cl-field__error')).toHaveText(
      `The repository ${other} has the forge \`other\`. Only a github repository or a ` +
        'gitlab repository has issues to import.',
      { timeout: 10_000 },
    );
    expect((await call(token, 'GET', `/api/repositories/${other}`)).issue_import_interval).toBe(
      'off',
    );
  });
});

test('issue cadence: a person who is not an org admin sees it and cannot change it', async ({
  page,
}) => {
  test.setTimeout(180_000);
  const token = await mintToken();
  const slug = `cad-ro-${RUN}`;
  await createRepository(GUI, token, {
    slug,
    repoFullName: `acme/${slug}`,
    team: 'platform',
  });
  // Carol is a member. A grant on a board lets her reach the admin pages.
  const members = await readEachPage(GUI, token, '/api/members');
  const carol = members.find((m) => m.email === 'carol@kairos.test');
  expect(carol, 'carol is a member').toBeTruthy();
  const board = (await call(token, 'GET', '/api/boards/platform-delivery')).id as string;
  const had = (
    (await call(token, 'GET', `/api/members/${carol.user_id}/grants`)).boards as any[]
  ).find((b) => b.board_slug === 'platform-delivery')?.capabilities as string[];
  const hadConfigure = had.includes('configure_boards');
  try {
    if (!hadConfigure) {
      await call(token, 'PATCH', `/api/members/${carol.user_id}/grants`, {
        changes: [{ board_id: board, grant: ['configure_boards'], revoke: [] }],
      });
    }
    await login(page, 'carol@kairos.test', 'carol-password');
    await page.goto('/admin/repositories');
    await expect(row(page, slug)).toBeVisible({ timeout: 30_000 });
    await expect(select(page, slug)).toHaveValue('off');
    await expect(select(page, slug)).toBeDisabled();
    await expect(cadence(page, slug).getByRole('button', { name: 'Save interval' })).toBeDisabled();
    await expect(cadence(page, slug).locator('[data-testid="issue-cadence-readonly"]')).toHaveText(
      'Only an organization admin can change the interval.',
    );
    await expect(state(page, slug)).toContainText('Last complete run: none yet.');
  } finally {
    if (!hadConfigure) {
      await call(token, 'PATCH', `/api/members/${carol.user_id}/grants`, {
        changes: [{ board_id: board, grant: [], revoke: ['configure_boards'] }],
      }).catch(() => undefined);
    }
  }
});
