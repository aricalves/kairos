// KAIROS-T-0586 — the panel "External references" of the item page
// (KAIROS-T-0508):
//
//   1. a new task names no external document;
//   2. Add reference with a label: the label is the text of the link, the
//      URI shows below it, and the link opens a new tab with
//      rel="noopener noreferrer";
//   3. a reference with no label: the URI is the text of the link;
//   4. a scheme that the server refuses (javascript:) shows the refusal in
//      the panel, and adds nothing;
//   5. Remove takes one reference away; the server agrees;
//   6. carol, who cannot edit the task, reads the references with no add
//      form and no Remove.
//
// The task is new for each run; the spec archives it at the end.

import { test, expect, type Browser, type Page } from '@playwright/test';
import { mintToken } from '../helpers/auth';
import { createTask, tryArchiveTask } from '../helpers/api';

const GUI = process.env.E2E_GUI_BASE_URL ?? 'http://localhost:41080';
const RUN = Date.now().toString(36);

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

async function references(token: string, code: string): Promise<any[]> {
  const res = await fetch(`${GUI}/api/tasks/${code}/external`, {
    headers: { authorization: `Bearer ${token}` },
  });
  expect(res.ok, `GET external of ${code} -> ${res.status}`).toBeTruthy();
  return (await res.json()).external;
}

async function asCarol(browser: Browser, code: string): Promise<void> {
  const context = await browser.newContext();
  try {
    const page = await context.newPage();
    await login(page, 'carol@kairos.test', 'carol-password');
    await page.goto(`/items/${code}`);
    const panel = page.locator('[data-testid="external-refs"]');
    await expect(panel.getByRole('link', { name: `RFC ${RUN}` })).toBeVisible({ timeout: 30_000 });
    await expect(panel.locator('[data-testid="external-refs-add"]')).toHaveCount(0);
    await expect(panel.getByRole('button', { name: 'Remove', exact: true })).toHaveCount(0);
  } finally {
    await context.close();
  }
}

test('external references: add with and without a label, a refusal, remove', async ({
  page,
  browser,
}) => {
  // Each page load compiles the wasm bundle of the app again.
  test.setTimeout(180_000);
  const token = await mintToken();
  const task = await createTask(GUI, token, {
    title: `E2E external refs ${RUN}`,
    boardId: 'platform-delivery',
  });
  const code: string = task.short_code;
  const labelled = `https://example.com/wiki/rfc-${RUN}`;
  const bare = `https://example.com/design/${RUN}`;
  const panel = page.locator('[data-testid="external-refs"]');
  const form = panel.locator('[data-testid="external-refs-add"]');
  const add = form.getByRole('button', { name: 'Add reference', exact: true });

  try {
    await test.step('login, open the task: no reference yet', async () => {
      await login(page);
      await page.goto(`/items/${code}`);
      await expect(panel).toContainText('This item names no external document.', {
        timeout: 30_000,
      });
      await expect(add).toBeDisabled();
    });

    await test.step('a reference with a label', async () => {
      await form.getByLabel('URI').fill(labelled);
      await form.getByLabel('Label').fill(`RFC ${RUN}`);
      await add.click();
      const row = panel.locator(`[data-external="${labelled}"]`);
      const link = row.getByRole('link', { name: `RFC ${RUN}` });
      await expect(link).toBeVisible();
      await expect(link).toHaveAttribute('href', labelled);
      await expect(link).toHaveAttribute('target', '_blank');
      await expect(link).toHaveAttribute('rel', 'noopener noreferrer');
      await expect(row).toContainText(labelled);
      // The form is empty again.
      await expect(form.getByLabel('URI')).toHaveValue('');
      await expect(form.getByLabel('Label')).toHaveValue('');
    });

    await test.step('a reference with no label: the URI is the text', async () => {
      await form.getByLabel('URI').fill(bare);
      await add.click();
      const row = panel.locator(`[data-external="${bare}"]`);
      await expect(row.getByRole('link', { name: bare, exact: true })).toBeVisible();
      expect((await references(token, code)).map((r) => r.uri).sort()).toEqual(
        [bare, labelled].sort(),
      );
    });

    await test.step('the server refuses a javascript: URI, and the panel says so', async () => {
      await form.getByLabel('URI').fill('javascript:alert(1)');
      await add.click();
      await expect(
        panel.locator('.cl-alert', { hasText: 'Kairos did not change the reference' }),
      ).toBeVisible();
      expect(await references(token, code)).toHaveLength(2);
    });

    await test.step('Remove takes the bare reference away', async () => {
      await form.getByLabel('URI').fill('');
      await panel
        .locator(`[data-external="${bare}"]`)
        .getByRole('button', { name: 'Remove', exact: true })
        .click();
      await expect(panel.locator(`[data-external="${bare}"]`)).toHaveCount(0);
      await expect(panel.locator(`[data-external="${labelled}"]`)).toBeVisible();
      expect((await references(token, code)).map((r) => r.uri)).toEqual([labelled]);
    });

    await test.step('carol cannot edit the task: she reads, with no add and no Remove', async () => {
      await asCarol(browser, code);
    });
  } finally {
    await tryArchiveTask(GUI, token, code);
  }
});
