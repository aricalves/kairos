// KAIROS-T-0409 — an uploaded site cannot act as its reader (KAIROS-D-0012
// "Serving"). Kairos serves a repository's site on its own origin, with the
// CSP `sandbox` and no `allow-same-origin`: each page has an opaque origin.
//
// A test site is published whose script, read by a signed-in person, tries
// each escape and writes what happened into the page. Each one must fail:
//
// - read document.cookie (the session);
// - use localStorage;
// - fetch the API with credentials;
// - post a form to the API (it would make an agent key);
// - read a frame of the app.
//
// The same site's page, links and stylesheet still work. Runs in Chromium,
// Firefox and WebKit (playwright.config.ts).

import { test, expect, type Page } from '@playwright/test';
import { targz } from '../helpers/targz';

const EMAIL = process.env.E2E_LOCAL_EMAIL ?? 'alice@kairos.test';
const PASSWORD = process.env.E2E_LOCAL_PASSWORD ?? 'alice-local-password';
const ROOT = '/pages/demo/platform/repos/kairos-sandbox-test/';

const ESCAPES = `
(async function () {
  var out = {};
  try { out.cookie = document.cookie; } catch (e) { out.cookie = 'blocked: ' + e.name; }
  try { localStorage.setItem('k', 'v'); out.storage = 'used'; } catch (e) { out.storage = 'blocked: ' + e.name; }
  try {
    var r = await fetch('/api/whoami', { credentials: 'include' });
    out.fetch = 'status ' + r.status;
  } catch (e) { out.fetch = 'blocked: ' + e.name; }
  try {
    var frame = document.createElement('iframe');
    frame.src = '/boards';
    document.body.appendChild(frame);
    await new Promise(function (resolve) { setTimeout(resolve, 1500); });
    try { out.frame = 'read: ' + frame.contentDocument.title; } catch (e) { out.frame = 'blocked: ' + e.name; }
  } catch (e) { out.frame = 'blocked: ' + e.name; }
  document.getElementById('result').textContent = JSON.stringify(out);
})();
`;

const SITE = {
  'index.html':
    '<!doctype html><html><head><title>Sandbox test</title>' +
    '<link rel="stylesheet" href="style.css"></head><body>' +
    '<h1>Sandbox test</h1><a href="guide/">Guide</a>' +
    '<form id="post" method="post" action="/api/me/agent-keys" target="sink">' +
    '<input name="name" value="from-a-site"></form><iframe name="sink"></iframe>' +
    '<pre id="result"></pre><script src="escape.js"></script></body></html>',
  'escape.js': ESCAPES,
  'style.css': 'h1 { color: rgb(1, 2, 3); }',
  'guide/index.html': '<!doctype html><html><body><h1>Guide</h1></body></html>',
};

async function bearer(page: Page): Promise<string> {
  const response = await page.request.post('/api/login', {
    data: { email: EMAIL, password: PASSWORD },
  });
  expect(response.ok()).toBeTruthy();
  return (await response.json()).token;
}

async function publish(page: Page, token: string) {
  const headers = { authorization: `Bearer ${token}` };
  // The repository of the test site, on the platform team (idempotent).
  const existing = await page.request.get('/api/repositories/kairos-sandbox-test', { headers });
  if (!existing.ok()) {
    const created = await page.request.post('/api/repositories', {
      headers,
      data: {
        slug: 'kairos-sandbox-test',
        forge: 'other',
        repo_full_name: 'kairos-sandbox-test',
        repo_url: 'https://example.com/kairos-sandbox-test',
        team: 'platform',
      },
    });
    expect(created.ok(), await created.text()).toBeTruthy();
  }
  const on = await page.request.patch('/api/repositories/kairos-sandbox-test', {
    headers,
    data: { site_enabled: true },
  });
  expect(on.ok(), await on.text()).toBeTruthy();
  const put = await page.request.put('/api/repositories/kairos-sandbox-test/site', {
    headers: { ...headers, 'content-type': 'application/gzip' },
    data: targz(SITE),
  });
  expect(put.status(), await put.text()).toBe(201);
}

async function agentKeys(page: Page, token: string): Promise<string[]> {
  const response = await page.request.get('/api/me/agent-keys', {
    headers: { authorization: `Bearer ${token}` },
  });
  expect(response.ok()).toBeTruthy();
  const body = await response.json();
  const items = Array.isArray(body) ? body : body.items ?? [];
  return items.map((k: { name: string }) => k.name);
}

test('site sandbox: a site cannot act as the person who reads it', async ({ page }) => {
  const token = await bearer(page);
  await publish(page, token);

  // The person reads the site, signed in (the session cookie is in the context).
  await page.goto(ROOT);
  await expect(page.getByRole('heading', { name: 'Sandbox test' })).toBeVisible();

  // The site works: its stylesheet applies and its links go.
  const color = await page.evaluate(
    () => getComputedStyle(document.querySelector('h1')!).color,
  );
  expect(color).toBe('rgb(1, 2, 3)');

  // Each escape fails.
  await expect(page.locator('#result')).not.toBeEmpty({ timeout: 15_000 });
  const result = JSON.parse((await page.locator('#result').textContent()) ?? '{}');
  expect(result.cookie, 'no session in document.cookie').not.toContain('kairos_session');
  expect(result.storage).toMatch(/^blocked/);
  expect(result.fetch, 'the API does not answer as the reader').not.toBe('status 200');
  expect(result.frame).toMatch(/^blocked/);

  // A form posted to the API does not make an agent key. Wait for the
  // answer of the API, then look.
  const answered = page.waitForResponse(
    (response) =>
      response.url().endsWith('/api/me/agent-keys') && response.request().method() === 'POST',
  );
  await page.evaluate(() => (document.getElementById('post') as HTMLFormElement).submit());
  await answered;
  expect(await agentKeys(page, token)).not.toContain('from-a-site');

  // The links of the site work.
  await page.getByRole('link', { name: 'Guide' }).click();
  await expect(page.getByRole('heading', { name: 'Guide' })).toBeVisible();
});
