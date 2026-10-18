// KAIROS-T-0586 — Admin → Templates: the first content and the metadata
// fields of a new document.
//
//   1. Create template: a name, a slug, the starter content and a metadata
//      field (the seeded definition `priority`, default `high`); the page
//      lists it, and the server has each part;
//   2. Edit: the form loads the content and the field; a new content and
//      Remove field, then Save template; the server has the change;
//   3. Delete: the page and the server lose the template.
//
// The slug is new for each run. The spec deletes the template at the end
// if a step fails.

import { test, expect, type Page } from '@playwright/test';
import { mintToken } from '../helpers/auth';

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

async function api(token: string, path: string): Promise<any> {
  const res = await fetch(GUI + path, { headers: { authorization: `Bearer ${token}` } });
  expect(res.ok, `GET ${path} -> ${res.status}`).toBeTruthy();
  return res.json();
}

async function templateOf(token: string, slug: string): Promise<any | undefined> {
  const list = await api(token, '/api/templates?limit=200');
  const found = (list.items as any[]).find((t) => t.slug === slug);
  return found ? api(token, `/api/templates/${found.id}`) : undefined;
}

const panel = (page: Page, title: string) =>
  page.locator('.cl-panel', { has: page.locator('.cl-panel__title', { hasText: title }) });

test('admin templates: create with a metadata field, edit, delete', async ({ page }) => {
  // Each page load compiles the wasm bundle of the app again.
  test.setTimeout(180_000);
  const token = await mintToken();
  const name = `E2E Review ${RUN}`;
  const slug = `e2e-review-${RUN}`;
  const notice = page.locator('.cl-alert', { hasText: 'Done' });
  const row = page
    .locator('.cl-stack', { has: page.locator('code', { hasText: new RegExp(`^${slug}$`) }) })
    .last();

  try {
    await test.step('login, open Templates, create the template', async () => {
      await login(page);
      await page.goto('/admin/templates');
      const form = panel(page, 'Create template');
      await expect(form).toBeVisible({ timeout: 30_000 });
      await form.getByLabel('Name').fill(name);
      await form.getByLabel('Slug').fill(slug);
      await form.getByLabel('Starter content (markdown)').fill('## Goal\n\n## Risks\n');
      await form.getByRole('button', { name: 'Add metadata field', exact: true }).click();
      await form.getByLabel('Definition').selectOption('priority');
      await form.getByLabel('Default (optional)').fill('high');
      await form.getByRole('button', { name: 'Create template', exact: true }).click();
      await expect(notice).toContainText(`Kairos made the template "${name}".`);
      await expect(row).toContainText(name);
      const made = await templateOf(token, slug);
      expect(made, 'the template').toBeTruthy();
      expect(made.name).toBe(name);
      expect(made.content).toBe('## Goal\n\n## Risks\n');
      expect(
        (made.metadata as any[]).map((m) => `${m.slug}=${m.default_value}`),
      ).toEqual(['priority=high']);
    });

    await test.step('Edit loads the template; a new content, no field, Save', async () => {
      await row.getByRole('button', { name: 'Edit', exact: true }).click();
      const content = row.getByLabel('Content (markdown)');
      await expect(content).toHaveValue('## Goal\n\n## Risks\n');
      await expect(row.getByLabel('Definition')).toHaveValue('priority');
      await content.fill('## Goal\n\n## Risks\n\n## Rollback\n');
      await row.getByRole('button', { name: 'Remove field', exact: true }).click();
      await row.getByRole('button', { name: 'Save template', exact: true }).click();
      await expect(notice).toContainText(`Kairos changed the template "${name}".`);
      const changed = await templateOf(token, slug);
      expect(changed.content).toBe('## Goal\n\n## Risks\n\n## Rollback\n');
      expect(changed.metadata).toEqual([]);
    });

    await test.step('Delete: the template is gone', async () => {
      await row.getByRole('button', { name: 'Delete', exact: true }).click();
      await expect(notice).toContainText(`Kairos deleted the template "${name}".`);
      await expect(row).toHaveCount(0);
      expect(await templateOf(token, slug)).toBeUndefined();
    });
  } finally {
    const left = (await api(token, '/api/templates?limit=200')).items.find(
      (t: any) => t.slug === slug,
    );
    if (left) {
      await fetch(`${GUI}/api/templates/${left.id}`, {
        method: 'DELETE',
        headers: { authorization: `Bearer ${token}` },
      });
    }
  }
});
