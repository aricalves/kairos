// KAIROS-T-0587 — the admin pages for service accounts, the claim flags of
// a column, the board policies, a bucket initiative and a webhook rotate:
//
//   1. Admin → Members: make a service account, make a key (it shows once,
//      with Copy and the warning), rotate it (the grace time shows), revoke
//      the new key and delete the account, each after a confirmation;
//   2. Admin → Boards → a delivery board: the switch "Keeps claims" of Todo
//      goes on and off, the switch of the parent gate goes off and on, and
//      the switch of the blockers gate goes on and off (KAIROS-T-0652); the
//      initiative board shows the owner gate and no parent or blockers gate;
//   3. the initiative create dialog makes a bucket of the kind bug;
//   4. Admin → Repositories: Rotate gives a new webhook URL and secret.
//
// Each run uses its own names (a timestamp). Each test puts back what it
// changed, also when it fails.

import { test, expect, type Page } from '@playwright/test';
import { mintToken } from '../helpers/auth';
import { createForgeConnection, createRepository, loadBoard } from '../helpers/api';

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

async function api(token: string, method: string, path: string, body?: unknown): Promise<any> {
  const res = await fetch(GUI + path, {
    method,
    headers: { authorization: `Bearer ${token}`, 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  expect(res.ok, `${method} ${path} -> ${res.status}`).toBeTruthy();
  return res.json();
}

test('admin members: a service account, its keys, and the delete', async ({ page }) => {
  const token = await mintToken();
  const name = `e2e-sa-${RUN}`;
  const account = () => page.locator(`[data-testid="service-account"][data-account="${name}"]`);
  const secret = page.locator('[data-testid="service-key-secret"]');
  const confirm = async () => {
    await page.getByRole('dialog').getByRole('button', { name: 'Confirm', exact: true }).click();
  };

  try {
    await test.step('login, open Members, make the service account', async () => {
      await login(page);
      await page.goto('/admin/members');
      const panel = page.locator('.cl-panel', {
        has: page.locator('.cl-panel__title', { hasText: 'Service accounts' }),
      });
      await expect(panel).toBeVisible({ timeout: 30_000 });
      await panel.locator('.cl-field', { hasText: 'Name' }).first().locator('input').fill(name);
      await panel.getByRole('button', { name: 'Make service account', exact: true }).click();
      await expect(
        page.getByText(`Kairos made the service account ${name}. Make a key for it below.`),
      ).toBeVisible();
      await expect(account()).toContainText('The service account has no keys.');
    });

    await test.step('make a key: it shows once, with Copy and the warning', async () => {
      await account().locator('.cl-field', { hasText: 'Key name' }).locator('input').fill('nightly');
      await account().getByRole('button', { name: 'Make key', exact: true }).click();
      await expect(secret).toBeVisible({ timeout: 10_000 });
      await expect(secret).toContainText('Copy the key now. Kairos does not show it again.');
      await expect(secret.locator('code')).toContainText('kairos_');
      const shown = (await secret.locator('code').innerText()).trim();
      await secret.getByRole('button', { name: 'Hide the key' }).click();
      await expect(secret).toHaveCount(0);
      // The secret is gone from the page; the row shows the start only.
      expect(await page.content()).not.toContain(shown);
      await expect(account().locator('[data-testid="service-key"]')).toHaveCount(1);
      await expect(account().locator('[data-testid="service-key"]')).toContainText('active');
    });

    await test.step('rotate: a new secret, and the end of the old one', async () => {
      await account().getByRole('button', { name: 'Rotate', exact: true }).click();
      await expect(secret).toBeVisible({ timeout: 10_000 });
      await expect(secret).toContainText('The old secret works until');
      await secret.getByRole('button', { name: 'Hide the key' }).click();
      const keys = account().locator('[data-testid="service-key"]');
      await expect(keys).toHaveCount(2);
      await expect(keys.filter({ hasText: 'rotated' })).toHaveCount(1);
    });

    await test.step('revoke the new key after the confirmation', async () => {
      const fresh = account().locator('[data-testid="service-key"]', { hasText: 'active' });
      await fresh.getByRole('button', { name: 'Revoke', exact: true }).click();
      await expect(page.getByRole('dialog')).toContainText('You cannot undo a revoke.');
      await confirm();
      await expect(page.getByText(`Kairos revoked the key nightly of ${name}.`)).toBeVisible();
      await expect(
        account().locator('[data-testid="service-key"]', { hasText: 'revoked' }),
      ).toHaveCount(1);
    });

    await test.step('delete the service account after the confirmation', async () => {
      await account().getByRole('button', { name: 'Delete', exact: true }).click();
      await expect(page.getByRole('dialog')).toContainText('You cannot undo a delete.');
      await confirm();
      await expect(
        page.getByText(`Kairos deleted the service account ${name} and each key of it.`),
      ).toBeVisible();
      await expect(account()).toHaveCount(0);
    });
  } finally {
    const list = await api(token, 'GET', '/api/service-accounts');
    for (const left of (list.items as any[]).filter((a) => a.name === name)) {
      await fetch(`${GUI}/api/service-accounts/${left.id}`, {
        method: 'DELETE',
        headers: { authorization: `Bearer ${token}` },
      });
    }
  }
});

test('admin boards: the claim flags of a column and the board policies', async ({ page }) => {
  const token = await mintToken();
  const delivery = await loadBoard(GUI, token, (b) => b.slug === 'platform-delivery');
  const initiatives = await loadBoard(GUI, token, (b) => b.board_level === 'initiative');
  const todo = delivery.columnId.get('Todo')!;
  const row = (column: string) =>
    page.locator(`[data-testid="column-row"][data-column="${column}"]`);
  const column = async (id: string): Promise<any> =>
    (await api(token, 'GET', `/api/boards/${delivery.id}`)).columns.find((c: any) => c.id === id);

  try {
    await test.step('login, open the delivery board settings', async () => {
      await login(page);
      await page.goto(`/admin/boards/${delivery.id}`);
      await expect(row('Todo')).toBeVisible({ timeout: 30_000 });
      // The seed: Active gives claims, Blocked keeps claims.
      await expect(
        row('Active').locator('[data-testid="column-claims"] [role="switch"]'),
      ).toHaveAttribute('aria-checked', 'true');
      await expect(
        row('Blocked').locator('[data-testid="column-keeps-claims"] [role="switch"]'),
      ).toHaveAttribute('aria-checked', 'true');
    });

    await test.step('Keeps claims of Todo goes on, then off', async () => {
      const keeps = row('Todo').locator('[data-testid="column-keeps-claims"] [role="switch"]');
      await expect(keeps).toHaveAttribute('aria-checked', 'false');
      await keeps.click();
      await expect(page.getByText('The column keeps claims now.')).toBeVisible();
      expect((await column(todo)).keeps_claims).toBe(true);
      await row('Todo').locator('[data-testid="column-keeps-claims"] [role="switch"]').click();
      await expect(page.getByText('The column does not keep claims now.')).toBeVisible();
      expect((await column(todo)).keeps_claims).toBe(false);
    });

    await test.step('the parent gate of the delivery board goes off, then on', async () => {
      const gate = page.locator('[data-testid="parent_gate"] [role="switch"]');
      await expect(gate).toHaveAttribute('aria-checked', 'true');
      await expect(page.locator('[data-testid="owner_gate"]')).toHaveCount(0);
      await gate.click();
      await expect(page.getByText('Kairos turned the parent gate of the board off.')).toBeVisible();
      expect((await api(token, 'GET', `/api/boards/${delivery.id}`)).parent_gate).toBe(false);
      await page.locator('[data-testid="parent_gate"] [role="switch"]').click();
      await expect(page.getByText('Kairos turned the parent gate of the board on.')).toBeVisible();
      expect((await api(token, 'GET', `/api/boards/${delivery.id}`)).parent_gate).toBe(true);
    });

    await test.step('the blockers gate of the delivery board goes on, then off', async () => {
      // KAIROS-T-0652: the gate is off by default (KAIROS-A-0053).
      const gate = page.locator('[data-testid="blockers_gate"] [role="switch"]');
      await expect(gate).toHaveAttribute('aria-checked', 'false');
      await gate.click();
      await expect(page.getByText('Kairos turned the blockers gate of the board on.')).toBeVisible();
      expect((await api(token, 'GET', `/api/boards/${delivery.id}`)).blockers_gate).toBe(true);
      await page.locator('[data-testid="blockers_gate"] [role="switch"]').click();
      await expect(
        page.getByText('Kairos turned the blockers gate of the board off.'),
      ).toBeVisible();
      expect((await api(token, 'GET', `/api/boards/${delivery.id}`)).blockers_gate).toBe(false);
    });

    await test.step('the initiative board shows the owner gate only', async () => {
      await page.goto(`/admin/boards/${initiatives.id}`);
      await expect(page.locator('[data-testid="owner_gate"] [role="switch"]')).toBeVisible({
        timeout: 30_000,
      });
      await expect(page.locator('[data-testid="parent_gate"]')).toHaveCount(0);
      await expect(page.locator('[data-testid="blockers_gate"]')).toHaveCount(0);
    });
  } finally {
    await api(token, 'PATCH', `/api/boards/${delivery.id}/columns/${todo}`, {
      keeps_claims: false,
    });
    await api(token, 'PATCH', `/api/boards/${delivery.id}`, {
      parent_gate: true,
      blockers_gate: false,
    });
  }
});

test('the initiative create dialog makes a bucket', async ({ page }) => {
  const token = await mintToken();
  const title = `E2E bucket ${RUN}`;
  const card = () => page.locator('article.kairos-card', { hasText: title });
  let code: string | undefined;
  try {
    await login(page);
    await page.locator('.kairos-board-tile', { hasText: 'Initiatives' }).click();
    await page.waitForURL(/\/boards\/initiatives/);
    await page.getByRole('button', { name: 'New initiative', exact: true }).click();
    const modal = page.locator('.cl-modal');
    await modal.locator('input.cl-input').first().fill(title);
    const bucket = modal.locator('[data-testid="create-bucket"] select');
    await expect(bucket).toHaveValue('none');
    await bucket.selectOption('bug');
    await modal.getByRole('button', { name: 'Create' }).click();
    await expect(card()).toBeVisible({ timeout: 10_000 });
    await expect(card()).toContainText('bucket · bug');
    code = (await card().locator('a.kairos-card__code').innerText()).trim();
    expect(code, 'the new initiative').toBeTruthy();
    const initiative = await api(token, 'GET', `/api/initiatives/${code}`);
    expect(initiative.is_bucket).toBe(true);
    expect(initiative.bucket_type).toBe('bug');
  } finally {
    if (code) {
      await fetch(`${GUI}/api/initiatives/${code}`, {
        method: 'DELETE',
        headers: { authorization: `Bearer ${token}` },
      });
    }
  }
});

test('admin repositories: Rotate gives a new webhook URL and secret', async ({ page }) => {
  const token = await mintToken();
  const slug = `rotate-${RUN}`;
  await createRepository(GUI, token, { slug, repoFullName: `acme/${slug}`, team: 'platform' });
  const first = await createForgeConnection(GUI, token, slug);
  try {
    await login(page);
    await page.goto('/admin/repositories');
    const row = page.locator(`[data-repo="${slug}"]`).first();
    await expect(row).toContainText('webhooks', { timeout: 30_000 });
    await row.getByRole('button', { name: 'Rotate', exact: true }).click();
    const secret = page.locator('[data-testid="webhook-secret"]');
    await expect(secret).toBeVisible({ timeout: 10_000 });
    await expect(secret).toContainText('/webhooks/github/');
    await expect(secret).not.toContainText(first.webhookSecret);
    await expect(secret).not.toContainText(first.webhookUrl);
    await expect(
      page.getByText(`Kairos rotated the webhook of "${slug}". The old URL and secret stop now.`),
    ).toBeVisible();
    await secret.getByRole('button', { name: 'I have copied them' }).click();
    await expect(secret).toHaveCount(0);
  } finally {
    await fetch(`${GUI}/api/repositories/${slug}`, {
      method: 'DELETE',
      headers: { authorization: `Bearer ${token}` },
    });
  }
});
