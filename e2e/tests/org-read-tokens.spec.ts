// KAIROS-T-0586 — Admin → Code index → "Read tokens by host": the read
// tokens of the organization, one for each host (KAIROS-T-0431).
//
//   1. Save with no host is refused on the page, and nothing is sent;
//   2. Set a token: the confirmation names the host and says the new value
//      replaces a stored one; Save keeps it; the row names the host and who
//      set it, and the page never shows the token;
//   3. Remove, after its confirmation: the row is gone, and the server has
//      no token for the host.
//
// The host is new for each run (an example.org name). The spec removes it
// at the end if a step fails.

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

async function hosts(token: string): Promise<string[]> {
  const res = await fetch(`${GUI}/api/org/read-tokens`, {
    headers: { authorization: `Bearer ${token}` },
  });
  expect(res.ok, `GET /api/org/read-tokens -> ${res.status}`).toBeTruthy();
  return ((await res.json()) as any[]).map((t) => t.host);
}

test('org read tokens: set a token for a host, then remove it', async ({ page }) => {
  // Each page load compiles the wasm bundle of the app again.
  test.setTimeout(180_000);
  const token = await mintToken();
  const host = `git-${RUN}.example.org`;
  const secret = `e2e-read-token-${RUN}`;
  const panel = page.locator('[data-testid="code-index-read-tokens"]');
  const row = panel
    .locator('.cl-group', { hasText: host })
    .filter({ has: page.getByRole('button', { name: 'Remove', exact: true }) })
    .last();
  const dialog = page.getByRole('dialog');

  try {
    await test.step('login, open the code index page; no host is refused', async () => {
      await login(page);
      await page.goto('/admin/code-index');
      await expect(panel).toBeVisible({ timeout: 30_000 });
      await panel.getByTestId('code-index-read-token-add').click();
      await panel.getByLabel('Token').fill(secret);
      await panel.getByTestId('code-index-read-token-save').click();
      await dialog.getByRole('button', { name: 'Save', exact: true }).click();
      await expect(page.getByText('Write the host, for example github.com.')).toBeVisible();
      expect(await hosts(token)).not.toContain(host);
    });

    await test.step('Set a token: the confirmation, then the row', async () => {
      await panel.getByLabel('Host').fill(host);
      await panel.getByTestId('code-index-read-token-save').click();
      await expect(dialog).toContainText('Set the read token of this host?');
      await expect(dialog).toContainText(`Host: ${host}`);
      await expect(dialog).toContainText('the new value replaces a stored token of this host');
      await dialog.getByRole('button', { name: 'Save', exact: true }).click();
      await expect(page.getByText(`Kairos keeps the read token of ${host}.`)).toBeVisible();
      await expect(row).toContainText('Set by alice at');
      // The form is closed, and the token is nowhere on the page.
      await expect(panel.getByLabel('Token')).toHaveCount(0);
      expect(await page.content()).not.toContain(secret);
      expect(await hosts(token)).toContain(host);
    });

    await test.step('Remove, after the confirmation', async () => {
      await row.getByRole('button', { name: 'Remove', exact: true }).click();
      await expect(dialog).toContainText(`Remove the read token of ${host}?`);
      await dialog.getByRole('button', { name: 'Remove', exact: true }).click();
      await expect(page.getByText(`Kairos removed the read token of ${host}.`)).toBeVisible();
      await expect(row).toHaveCount(0);
      expect(await hosts(token)).not.toContain(host);
    });
  } finally {
    if ((await hosts(token)).includes(host)) {
      await fetch(`${GUI}/api/org/read-tokens/${encodeURIComponent(host)}`, {
        method: 'DELETE',
        headers: { authorization: `Bearer ${token}` },
      });
    }
  }
});
