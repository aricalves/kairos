// KAIROS-T-0370 — the security headers, and the GUI under its CSP:
//
//   1. the page of the GUI has a CSP (with 'wasm-unsafe-eval' and
//      frame-ancestors 'none'), X-Frame-Options DENY, nosniff and a
//      Referrer-Policy; no HSTS over plain HTTP;
//   2. an API response has the headers too;
//   3. the GUI logs in, and the pages (boards, a board, an item with a
//      mermaid diagram, search, activity, agent keys, admin) render with
//      no CSP violation.

import { test, expect, type Page } from '@playwright/test';
import { mintToken } from '../helpers/auth';
import { createTask, loadBoard } from '../helpers/api';

const SERVER = process.env.E2E_GUI_BASE_URL ?? 'http://localhost:41080';

async function login(page: Page): Promise<void> {
  await page.goto('/');
  await page.waitForSelector('#login', { timeout: 30_000 });
  await page.fill('#login', 'alice@kairos.test');
  await page.fill('#password', 'alice-password');
  await page.click('#submit-login');
  await page.waitForURL((url) => url.pathname.startsWith('/boards'), { timeout: 30_000 });
}

test('the server sends the security headers, and the GUI runs under its CSP', async ({ page }) => {
  // Each CSP violation of the page, from the first script on.
  await page.addInitScript(() => {
    (window as any).__csp = [];
    document.addEventListener('securitypolicyviolation', (e) => {
      (window as any).__csp.push(`${e.violatedDirective} ${e.blockedURI}`);
    });
  });
  const consoleErrors: string[] = [];
  page.on('console', (msg) => {
    if (msg.type() === 'error' && /Content Security Policy/i.test(msg.text())) {
      consoleErrors.push(msg.text());
    }
  });

  await test.step('the page of the GUI has the headers', async () => {
    const response = await page.goto('/');
    const headers = response!.headers();
    const csp = headers['content-security-policy'];
    expect(csp).toContain("script-src 'self' 'wasm-unsafe-eval' 'sha256-");
    expect(csp).toContain("frame-ancestors 'none'");
    expect(csp).toContain("object-src 'none'");
    expect(headers['x-frame-options']).toBe('DENY');
    expect(headers['x-content-type-options']).toBe('nosniff');
    expect(headers['referrer-policy']).toBe('same-origin');
    expect(headers['strict-transport-security']).toBeUndefined();
  });

  await test.step('an API response has the headers', async () => {
    const res = await fetch(`${SERVER}/api/config`);
    expect(res.headers.get('x-content-type-options')).toBe('nosniff');
    expect(res.headers.get('x-frame-options')).toBe('DENY');
    expect(res.headers.get('content-security-policy')).toBe("frame-ancestors 'none'");
  });

  const token = await mintToken();
  const board = await loadBoard(SERVER, token, (b) => b.slug === 'platform-delivery');
  const task = await createTask(SERVER, token, {
    title: `E2E CSP ${Date.now()}`,
    boardId: board.id,
    content: ['```mermaid', 'graph TD', '  A --> B', '```', '', 'The end.'].join('\n'),
  });

  try {
    await test.step('the pages render with no CSP violation', async () => {
      await login(page);
      await expect(page.locator('.kairos-board-tile').first()).toBeVisible();
      await page.locator('a[href="/boards/platform-delivery"]').first().click();
      await expect(page.locator('article.kairos-card').first()).toBeVisible();
      await page.goto(`/items/${task.short_code}`);
      await expect(
        page.locator('.kairos-editor .kairos-markdown .kairos-mermaid-diagram svg'),
      ).toHaveCount(1, { timeout: 15_000 });
      for (const [path, text] of [
        ['/search', 'Search'],
        ['/activity', 'Activity'],
        ['/agent-keys', 'Agent keys'],
        ['/admin', 'Admin'],
      ]) {
        await page.goto(path);
        await expect(page.getByText(text).first()).toBeVisible();
      }
      const violations = await page.evaluate(() => (window as any).__csp as string[]);
      expect(violations).toEqual([]);
      expect(consoleErrors).toEqual([]);
    });
  } finally {
    await fetch(`${SERVER}/api/tasks/${task.short_code}`, {
      method: 'DELETE',
      headers: { authorization: `Bearer ${token}` },
    });
  }
});
