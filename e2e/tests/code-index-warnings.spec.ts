// KAIROS-T-0478 — the SCIP warnings of a run and the disk use of the
// builder, in the code index panel of a repository:
//
//   1. REAL PKCE login (alice, an organization admin)
//   2. the answer of `GET …/code-indexes/builds` is replaced (page.route)
//      with one `ok` run that has a SCIP warning for Go, and the disk use
//      of the builder: the test server has no tools and builds no Go code
//   3. the panel shows the warning line under the run, and the disk use
//   4. carol, a member, sees the warning, and no disk use (the server gives
//      it to an admin only; the panel shows it to an admin only)
//
// The spec writes nothing.

import { test, expect, type Page } from '@playwright/test';

async function login(page: Page, email: string, password: string): Promise<void> {
  await page.goto('/');
  await page.waitForSelector('#login', { timeout: 30_000 });
  await page.fill('#login', email);
  await page.fill('#password', password);
  await page.click('#submit-login');
  await page.waitForURL((url) => url.pathname.startsWith('/boards'), {
    timeout: 30_000,
  });
}

const RUNS = {
  items: [
    {
      id: '00000000-0000-0000-0000-000000000478',
      commit: '0123456789abcdef0123456789abcdef01234567',
      trigger: 'push',
      outcome: 'ok',
      symbols: 120,
      started_at: '2026-10-15T00:00:00Z',
      finished_at: '2026-10-15T00:01:00Z',
      scip_warnings: [{ language: 'go', reason: 'go mod download failed (exit status: 1): no network' }],
    },
  ],
  total: 1,
  limit: 5,
};

const DISK = {
  tools: [
    { name: 'go-1.27.2', language: 'go', bytes: 250_000_000 },
    { name: 'scip-go-0.2.7', language: 'go', bytes: 13_000_000 },
  ],
  caches: [{ language: 'go', repositories: 2, bytes: 120_000_000 }],
  caches_bytes: 120_000_000,
  caches_cap_bytes: 10_737_418_240,
  free_bytes: 70_000_000_000,
};

async function withRuns(page: Page, disk: boolean) {
  await page.route('**/api/repositories/*/code-indexes/builds*', (route) =>
    route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify(disk ? { ...RUNS, disk: DISK } : RUNS),
    }),
  );
}

async function openPanel(page: Page, slug: string) {
  const panel = page
    .locator(`[data-testid="code-index-panel"][data-code-index-repo="${slug}"]`)
    .first();
  await expect(panel).toBeVisible({ timeout: 15_000 });
  await panel.getByRole('button', { name: 'Show code index' }).click();
  await expect(panel.locator('[data-testid="code-index-runs"]')).toBeVisible({ timeout: 10_000 });
  return panel;
}

test('code index panel: the SCIP warning of a run, and the disk use for an admin', async ({
  page,
  browser,
}) => {
  await test.step('alice, an admin, sees the warning and the disk use', async () => {
    await withRuns(page, true);
    await login(page, 'alice@kairos.test', 'alice-password');
    await page.goto('/teams/web');
    const panel = await openPanel(page, 'portal-web');
    const warning = panel.locator('[data-testid="code-index-run-warning"][data-language="go"]');
    await expect(warning).toHaveText(
      'No SCIP index for go: go mod download failed (exit status: 1): no network. Its edges are by name.',
    );
    const disk = panel.locator('[data-testid="code-index-disk"]');
    await expect(disk).toContainText('Tools, go: 263.0 MB (go-1.27.2, scip-go-0.2.7)');
    await expect(disk).toContainText('Dependency caches: 120.0 MB of 10.7 GB');
    await expect(disk).toContainText('Free disk space: 70.0 GB');
  });

  await test.step('carol, a member, sees the warning and no disk use', async () => {
    const context = await browser.newContext();
    const carol = await context.newPage();
    await withRuns(carol, false);
    await login(carol, 'carol@kairos.test', 'carol-password');
    await carol.goto('/teams/web');
    const panel = await openPanel(carol, 'portal-web');
    await expect(panel.locator('[data-testid="code-index-run-warning"]')).toHaveCount(1);
    await expect(panel.locator('[data-testid="code-index-disk"]')).toHaveCount(0);
    await context.close();
  });
});
