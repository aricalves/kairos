// KAIROS-T-0586 — the panel "Suggested links" of the item page
// (KAIROS-T-0192): an agent proposes an edge (`propose_edge` over MCP),
// and a person confirms or rejects it on the item.
//
//   1. an agent proposes "A blocks B" and "A blocks C";
//   2. B shows the proposal from its side ("A blocks this"), with the why;
//   3. A shows both; Confirm makes the edge A blocks B, and the proposal
//      leaves the panel;
//   4. Reject leaves no edge to C; the panel has nothing left, so it is
//      gone; the server lists no pending proposal.
//
// The tasks are new for each run; the spec archives them at the end.

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

async function api(token: string, method: string, path: string): Promise<any> {
  const res = await fetch(GUI + path, { method, headers: { authorization: `Bearer ${token}` } });
  expect(res.ok, `${method} ${path} -> ${res.status}`).toBeTruthy();
  return res.json();
}

/** One JSON-RPC message from a streamable-HTTP answer (JSON or SSE). */
function rpcMessage(body: string): any {
  try {
    return JSON.parse(body);
  } catch {
    const line = body.split('\n').find((l) => l.startsWith('data:'));
    return JSON.parse(line!.slice(5).trim());
  }
}

/** `propose_edge` over /mcp, as an agent calls it. */
async function proposeEdge(token: string, args: Record<string, string>): Promise<void> {
  const post = async (body: unknown) => {
    const res = await fetch(`${GUI}/mcp`, {
      method: 'POST',
      headers: {
        authorization: `Bearer ${token}`,
        accept: 'application/json, text/event-stream',
        'content-type': 'application/json',
      },
      body: JSON.stringify(body),
    });
    return { status: res.status, text: await res.text() };
  };
  const init = await post({
    jsonrpc: '2.0',
    id: 0,
    method: 'initialize',
    params: {
      protocolVersion: '2025-06-18',
      capabilities: {},
      clientInfo: { name: 'kairos-e2e', version: '0.0.0' },
    },
  });
  expect(init.status, init.text).toBe(200);
  await post({ jsonrpc: '2.0', method: 'notifications/initialized' });
  const call = await post({
    jsonrpc: '2.0',
    id: 1,
    method: 'tools/call',
    params: { name: 'propose_edge', arguments: args },
  });
  expect(call.status, call.text).toBe(200);
  const result = rpcMessage(call.text).result;
  expect(result?.isError, JSON.stringify(result)).toBeFalsy();
}

/** The codes that `code` blocks, from the server. */
async function blocks(token: string, code: string): Promise<string[]> {
  const body = await api(token, 'GET', `/api/tasks/${code}/relationships`);
  return (body.outgoing ?? [])
    .filter((g: any) => g.relationship === 'blocks')
    .flatMap((g: any) => g.items.map((i: any) => i.short_code as string));
}

test('suggested links: an agent proposes, a person confirms one and rejects one', async ({
  page,
}) => {
  // Each page load compiles the wasm bundle of the app again.
  test.setTimeout(180_000);
  const token = await mintToken();
  const made = async (name: string) =>
    (
      await createTask(GUI, token, {
        title: `E2E proposal ${name} ${RUN}`,
        boardId: 'platform-delivery',
      })
    ).short_code as string;
  const a = await made('A');
  const b = await made('B');
  const c = await made('C');
  const panel = page.locator('.cl-panel', {
    has: page.locator('.cl-panel__title', { hasText: 'Suggested links' }),
  });
  const proposal = (other: string) =>
    panel.locator('.cl-stack', { has: page.getByRole('link', { name: other, exact: true }) }).last();

  try {
    await test.step('an agent proposes two blocks edges (MCP)', async () => {
      await proposeEdge(token, {
        source: a,
        target: b,
        relationship: 'blocks',
        why: `B reads the table that A adds (${RUN}).`,
      });
      await proposeEdge(token, {
        source: a,
        target: c,
        relationship: 'blocks',
        why: `C names the same endpoint as A (${RUN}).`,
      });
      expect(await api(token, 'GET', `/api/items/${a}/proposals`)).toHaveLength(2);
    });

    await test.step('B shows the proposal from its side', async () => {
      await login(page);
      await page.goto(`/items/${b}`);
      await expect(panel).toBeVisible({ timeout: 30_000 });
      await expect(proposal(a)).toContainText(`${a} blocks this`);
      await expect(proposal(a)).toContainText('B reads the table that A adds');
    });

    await test.step('A shows both; Confirm makes the edge', async () => {
      await page.goto(`/items/${a}`);
      await expect(proposal(b)).toContainText(`this blocks ${b}`, { timeout: 30_000 });
      await expect(proposal(c)).toContainText(`this blocks ${c}`);
      await proposal(b).getByRole('button', { name: 'Confirm', exact: true }).click();
      await expect(panel.getByRole('link', { name: b, exact: true })).toHaveCount(0);
      await expect(proposal(c)).toBeVisible();
      expect(await blocks(token, a)).toEqual([b]);
    });

    await test.step('Reject leaves no edge, and the empty panel goes', async () => {
      await proposal(c).getByRole('button', { name: 'Reject', exact: true }).click();
      await expect(panel).toHaveCount(0);
      expect(await blocks(token, a)).toEqual([b]);
      expect(await api(token, 'GET', `/api/items/${a}/proposals`)).toHaveLength(0);
    });
  } finally {
    for (const code of [a, b, c]) await tryArchiveTask(GUI, token, code);
  }
});
