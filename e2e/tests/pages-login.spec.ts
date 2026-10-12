// KAIROS-T-0403 — the docs sites send a reader with no session to the login page,
// and the login page sends them back.
//
// The session cookie is SameSite=Strict, so a link to a docs site page from another
// site (a chat app) arrives without it. The server answers 302 to /login?next=<path>.
// After a sign-in the reader lands on that path with a full page load (the server
// renders it, not the app). A reader who is signed in already goes straight back.
// A `next` outside /pages/ is not followed.

import { test, expect } from '@playwright/test';

const EMAIL = process.env.E2E_LOCAL_EMAIL ?? 'alice@kairos.test';
const PASSWORD = process.env.E2E_LOCAL_PASSWORD ?? 'alice-local-password';
// The GUI server pins KAIROS_SINGLE_TENANT=demo.
const LANDING = '/pages/demo/';
// The path of the page itself: `/login?next=/pages/demo/` ends with the landing too.
const ON_LANDING = /^https?:\/\/[^/]+\/pages\/demo\/$/;

async function logIn(page: import('@playwright/test').Page) {
  await expect(page.getByRole('button', { name: 'Log in' })).toBeVisible();
  await page.getByLabel('Email').fill(EMAIL);
  await page.getByLabel('Password').fill(PASSWORD);
  await page.getByLabel('Password').press('Enter');
}

test('pages: a reader with no session signs in and lands on the docs site page', async ({
  page,
}) => {
  page.on('console', (m) => { if (m.type() !== 'warning') console.log('CONSOLE', m.type(), m.text()); });
  page.on('response', (r) => console.log('RES', r.status(), r.url()));
  await page.goto(LANDING);
  await expect(page).toHaveURL(/\/login\?next=\/pages\/demo\/$/);

  await logIn(page);

  await expect(page).toHaveURL(ON_LANDING);
  await expect(page.getByRole('heading', { level: 1, name: 'demo' })).toBeVisible();

  // Signed in already: the login page sends the reader straight back, also when
  // `next` is percent-encoded.
  await page.goto(`/login?next=${encodeURIComponent(LANDING)}`);
  await expect(page).toHaveURL(ON_LANDING);
});

test('pages: the login page does not follow a next outside the docs sites', async ({
  page,
}) => {
  await page.goto('/login?next=https://example.com/pages/');
  await logIn(page);
  await expect(page.getByRole('link', { name: 'Boards', exact: true })).toBeVisible();
  expect(new URL(page.url()).host).not.toBe('example.com');
});
