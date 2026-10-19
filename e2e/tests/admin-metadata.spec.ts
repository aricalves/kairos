// KAIROS-T-0586 — Admin → Metadata definitions: the typed fields that an
// item can have.
//
//   1. an enum definition needs one value or more: the server refuses one
//      with none, and the page shows the refusal;
//   2. Create definition (enum): values are added, one is taken out with ×,
//      and the page lists the definition with its values in order;
//   3. Edit: a new name and one more value, Save; the server has both;
//   4. a definition that an item uses is not deleted (DEFINITION_IN_USE);
//   5. once no item uses it, Delete removes it from the page and the
//      server.
//
// The slug is new for each run. The spec deletes what is left at the end.

import { test, expect, type Page } from '@playwright/test';
import { mintToken } from '../helpers/auth';
import { createTask, tryArchiveTask } from '../helpers/api';

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

async function call(token: string, method: string, path: string, body?: unknown) {
  const res = await fetch(GUI + path, {
    method,
    headers: { authorization: `Bearer ${token}`, 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await res.text();
  return { status: res.status, body: text ? JSON.parse(text) : null };
}

async function definitionOf(token: string, slug: string): Promise<any | undefined> {
  const list = await call(token, 'GET', '/api/metadata-definitions?limit=200');
  expect(list.status).toBe(200);
  return (list.body.items as any[]).find((d) => d.slug === slug);
}

const panel = (page: Page, title: string) =>
  page.locator('.cl-panel', { has: page.locator('.cl-panel__title', { hasText: title }) });

test('admin metadata: create an enum definition, edit it, a refused delete, delete', async ({
  page,
}) => {
  // Each page load compiles the wasm bundle of the app again.
  test.setTimeout(180_000);
  const token = await mintToken();
  const name = `E2E Risk ${RUN}`;
  const slug = `e2e_risk_${RUN}`;
  const notice = page.locator('.cl-alert', { hasText: 'Done' });
  const form = panel(page, 'Create definition');
  const row = page
    .locator('.cl-stack', { has: page.locator('code', { hasText: new RegExp(`^${slug}$`) }) })
    .last();
  let task: string | undefined;

  try {
    await test.step('login, open the page; an enum with no value is refused', async () => {
      await login(page);
      await page.goto('/admin/metadata');
      await expect(form).toBeVisible({ timeout: 30_000 });
      await form.getByLabel('Name').fill(name);
      await form.getByLabel('Slug').fill(slug);
      await form.getByLabel('Type').selectOption('enum');
      await form.getByRole('button', { name: 'Create definition', exact: true }).click();
      await expect(page.locator('.cl-alert[role="alert"]').first()).toBeVisible();
      await expect(notice).toHaveCount(0);
      expect(await definitionOf(token, slug)).toBeUndefined();
    });

    await test.step('values in order, one taken out, then Create definition', async () => {
      for (const value of ['low', 'medium', 'none', 'high']) {
        await form.getByPlaceholder('New option').fill(value);
        await form.getByRole('button', { name: 'Add option', exact: true }).click();
      }
      // Take "none" out again.
      await form
        .locator('.cl-group', { has: page.locator('.cl-pill', { hasText: /^none$/ }) })
        .last()
        .getByRole('button', { name: '×' })
        .click();
      await form.getByRole('button', { name: 'Create definition', exact: true }).click();
      await expect(notice).toContainText(`Kairos made the definition "${name}" (enum).`);
      await expect(row).toContainText('enum');
      expect(await row.locator('.cl-pill').allInnerTexts()).toEqual(
        expect.arrayContaining(['low', 'medium', 'high']),
      );
      const made = await definitionOf(token, slug);
      expect(made.field_type).toBe('enum');
      expect(made.enum_options).toEqual(['low', 'medium', 'high']);
    });

    await test.step('Edit: a new name and one more value', async () => {
      await row.getByRole('button', { name: 'Edit', exact: true }).click();
      await row.getByLabel('Name').fill(`${name} level`);
      await row.getByPlaceholder('New option').fill('critical');
      await row.getByRole('button', { name: 'Add option', exact: true }).click();
      await row.getByRole('button', { name: 'Save', exact: true }).click();
      await expect(notice).toContainText(`Kairos changed the definition "${name} level".`);
      const changed = await definitionOf(token, slug);
      expect(changed.name).toBe(`${name} level`);
      expect(changed.enum_options).toEqual(['low', 'medium', 'high', 'critical']);
    });

    await test.step('a definition that a task uses is not deleted', async () => {
      const made = await createTask(GUI, token, {
        title: `E2E metadata ${RUN}`,
        boardId: 'platform-delivery',
      });
      task = made.short_code;
      const set = await call(token, 'PATCH', `/api/tasks/${task}/metadata`, {
        values: { [slug]: 'high' },
      });
      expect(set.status, JSON.stringify(set.body)).toBe(200);
      await row.getByRole('button', { name: 'Delete', exact: true }).click();
      const alert = page.locator('.cl-alert[role="alert"]', { hasText: 'DEFINITION_IN_USE' });
      await expect(alert).toBeVisible();
      expect(await definitionOf(token, slug)).toBeTruthy();
    });

    await test.step('with no item on it, Delete removes the definition', async () => {
      const cleared = await call(token, 'PATCH', `/api/tasks/${task}/metadata`, {
        values: { [slug]: null },
      });
      expect(cleared.status, JSON.stringify(cleared.body)).toBe(200);
      await row.getByRole('button', { name: 'Delete', exact: true }).click();
      await expect(notice).toContainText(`Kairos deleted the definition "${name} level".`);
      await expect(row).toHaveCount(0);
      expect(await definitionOf(token, slug)).toBeUndefined();
    });
  } finally {
    if (task) await tryArchiveTask(GUI, token, task);
    const left = await definitionOf(token, slug);
    if (left) await call(token, 'DELETE', `/api/metadata-definitions/${left.id}`);
  }
});
