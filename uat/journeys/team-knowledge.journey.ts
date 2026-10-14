// J6 — "A team writes down how it works" (KAIROS-I-0013 D4). KAIROS-I-0007
// gave teams a documentation tree, a charter and announcements, and until
// now no journey touched any of it.
//
// The story is the one that makes a team space worth having: someone
// writes the thing down, someone else edits the same page at the same
// time and nobody loses work, the team gets told, and a person from
// another team can read it but not rewrite it.
import { expect } from '@playwright/test';
import { named } from '../run/context';
import { journey, step } from '../run/narrate';
import { openTeam, panel } from '../surfaces/gui';

const TEAM = process.env.UAT_TEAM ?? 'platform';
// A repository of TEAM, for its published docs site (KAIROS-T-0408).
const REPO = process.env.UAT_PLATFORM_REPO ?? 'payments-api';

journey(
  'team-knowledge',
  'A team writes down how it works',
  { humans: ['alice', 'bob', 'carol'] },
  async ({ cast, ledger }) => {
    const alice = cast.human('alice');
    const bob = cast.human('bob');
    const carol = cast.human('carol');

    // This journey edits the team's REAL seeded page (the thing under test
    // is "can a team keep its own docs current", not "can it hold a
    // throwaway"), so teardown puts the original text back — otherwise a
    // repeated --server run would pile edits onto a live deployment's
    // documentation.
    await step(alice, 'notes the current text of the how-to, to restore afterwards', async () => {
      const api = await alice.api();
      const team = await api.teamBySlug(TEAM);
      const pages = (await api.get(`/api/teams/${team.id}/pages`)) as any[];
      const target = pages.find((p: any) => p.title === 'Deploy Kairos');
      expect(target, 'the seeded how-to page exists').toBeTruthy();
      const original = target.content as string;
      ledger.add({
        kind: 'team-page',
        label: `${TEAM}/Deploy Kairos`,
        delete: async () => {
          const now = (await api.get(`/api/teams/${team.id}/pages`)).find(
            (p: any) => p.id === target.id,
          );
          await api.patch(`/api/teams/${team.id}/pages/${target.id}`, {
            title: now.title,
            content: original,
            version: now.version,
          });
        },
      });
      return { page: target.title, restore_to_version: target.version };
    });

    await step(bob, 'finds his team in the directory', async () => {
      const page = await bob.gui();
      await page.goto('/teams');
      // The directory is a grid of team tiles linking to /teams/<slug>.
      const tile = page.locator(`.kairos-board-tile a[href="/teams/${TEAM}"]`);
      await expect(tile).toBeVisible();
      await tile.click();
      await page.waitForURL(new RegExp(`/teams/${TEAM}`));
      await expect(panel(page, 'Charter').locator('.kairos-markdown')).toBeVisible();
      return { directory: '/teams', opened: TEAM };
    });

    // The seeded tree is the team's real documentation; bob edits a page
    // in it rather than creating a throwaway, because the thing under test
    // is "can a team keep its own docs current".
    const addition = named('checked by UAT');
    let pageVersion = 0;
    await step(bob, 'opens a how-to in the docs tree and adds a line to it', async () => {
      const page = await bob.gui();
      const docs = panel(page, 'Documentation');
      const folder = docs.locator('details.kairos-doctree__folder', {
        has: page.locator('summary', { hasText: 'Documentation' }),
      });
      if (!(await folder.getAttribute('open'))) await folder.locator('summary').first().click();
      const howTo = folder.locator('details.kairos-doctree__folder', {
        has: page.locator('summary', { hasText: 'How-to Guides' }),
      });
      if (!(await howTo.getAttribute('open'))) await howTo.locator('summary').first().click();
      await howTo.getByRole('link', { name: 'Deploy Kairos', exact: true }).click();
      await page.waitForURL(/\/teams\/.*\/pages\//);
      const content = panel(page, 'Content');
      await content.getByRole('button', { name: 'Edit', exact: true }).click();
      const editor = page.locator('.kairos-editor');
      const textarea = editor.locator('textarea.kairos-editor__textarea');
      await expect(textarea).toBeVisible();
      await textarea.fill(`${await textarea.inputValue()}\n\n${addition}\n`);
      await editor.getByRole('button', { name: 'Save' }).click();
      await expect(content.locator('.kairos-markdown')).toContainText(addition, { timeout: 15_000 });
      const pill = await content.locator('.cl-pill', { hasText: /^v\d+$/ }).innerText();
      pageVersion = Number(pill.replace('v', ''));
      return { page: 'Deploy Kairos', added: addition, version: pageVersion };
    });

    await step(bob, 'edits it again while alice saves the same page — and is told, not overwritten', async () => {
      const page = await bob.gui();
      const content = panel(page, 'Content');
      await content.getByRole('button', { name: 'Edit', exact: true }).click();
      const editor = page.locator('.kairos-editor');
      const textarea = editor.locator('textarea.kairos-editor__textarea');
      await expect(textarea).toBeVisible();
      await textarea.fill(`${await textarea.inputValue()}\n\nBob's second thought.\n`);

      // alice (org admin) writes the same page from her own session,
      // mid-edit. It cannot be carol: she is not a platform member, which
      // is the refusal the last step asserts.
      const api = await alice.api();
      const team = await api.teamBySlug(TEAM);
      const pages = (await api.get(`/api/teams/${team.id}/pages`)) as any[];
      const target = pages.find((p: any) => p.title === 'Deploy Kairos');
      expect(target, 'the seeded how-to page exists').toBeTruthy();
      await api.patch(`/api/teams/${team.id}/pages/${target.id}`, {
        title: target.title,
        content: `${target.content}\n\nAlice got here first.\n`,
        version: target.version,
      });

      await editor.getByRole('button', { name: 'Save' }).click();
      const dialog = page.locator('[role="dialog"]');
      await expect(dialog).toBeVisible({ timeout: 15_000 });
      await expect(dialog.getByText('Edit conflict')).toBeVisible();
      await dialog.getByRole('button', { name: 'Take theirs' }).click();
      await expect(dialog).toBeHidden();
      // "Take theirs" loads the server copy into the editor rather than
      // discarding the session — bob decides what to do with it next.
      await expect(textarea).toHaveValue(/Alice got here first\./);
      return { conflict: 'Edit conflict', resolved: 'take theirs', her_line_kept: true };
    });

    let announcement = '';
    await step(bob, 'tells the team about it', async () => {
      const page = await bob.gui();
      await openTeam(page, TEAM);
      announcement = named('deploy notes are current again');
      const announcements = panel(page, 'Announcements');
      await announcements.locator('textarea').fill(announcement);
      await announcements.getByRole('button', { name: 'Post' }).click();
      await expect(announcements.getByText(announcement)).toBeVisible({ timeout: 15_000 });
      const api = await alice.api();
      const team = await api.teamBySlug(TEAM);
      const posted = ((await api.get(`/api/teams/${team.id}/announcements`)) as any[]).find(
        (a: any) => (a.body ?? a.content ?? '').includes(announcement),
      );
      if (posted) {
        ledger.add({
          kind: 'announcement',
          label: announcement,
          delete: async () => {
            await api.delete(`/api/teams/${team.id}/announcements/${posted.id}`);
          },
        });
      }
      return { posted: announcement, ledgered: !!posted };
    });

    await step(alice, 'reads the announcement on the team page', async () => {
      const page = await alice.gui();
      await openTeam(page, TEAM);
      await expect(panel(page, 'Announcements').getByText(announcement)).toBeVisible({
        timeout: 15_000,
      });
      return { seen_by: 'alice' };
    });

    await step(carol, `can read ${TEAM}'s space but is offered nothing to change`, async () => {
      const page = await carol.gui();
      await openTeam(page, TEAM);
      // Reads are open tenant-wide; writing is the team's own business.
      await expect(panel(page, 'Charter').locator('.kairos-markdown')).toBeVisible();
      await expect(panel(page, 'Announcements').locator('textarea')).toHaveCount(0);
      await expect(panel(page, 'Charter').getByRole('button', { name: 'Edit' })).toHaveCount(0);
      return { can_read: true, write_controls_offered: 0 };
    });

    // KAIROS-T-0434: an agent keeps the team page in order through MCP, as
    // the onboarding skill does: it reads the tree, makes a folder and a
    // page in it, edits the page, moves it, and archives both.
    await step(alice, 'has her agent file a runbook in a new folder of the team page', async () => {
      const mcp = await alice.mcp();
      const tree = await mcp.call('team_pages', { team: TEAM });
      expect(tree).toContain('[charter]');
      const folderSlug = named('runbooks').toLowerCase().replace(/[^a-z0-9-]/g, '-').slice(0, 60);
      const api = await alice.api();
      const team = await api.teamBySlug(TEAM);
      const archive = async (path: string) => {
        const pages = (await api.get(`/api/teams/${team.id}/pages`)) as any[];
        const byPath = (p: string) => {
          let parent: string | null = null;
          let found: any;
          for (const slug of p.split('/')) {
            found = pages.find((x: any) => (x.parent_id ?? null) === parent && x.slug === slug);
            if (!found) return undefined;
            parent = found.id;
          }
          return found;
        };
        const node = byPath(path);
        if (node) await api.delete(`/api/teams/${team.id}/pages/${node.id}`);
      };
      ledger.add({
        kind: 'team-page',
        label: `${TEAM}/${folderSlug}`,
        delete: async () => {
          await archive('restart-the-worker');
          await archive(`${folderSlug}/restart-the-worker`);
          await archive(folderSlug);
        },
      });
      const created = await mcp.call('create_team_page', {
        team: TEAM,
        kind: 'folder',
        title: 'Runbooks',
        slug: folderSlug,
      });
      expect(created).toContain(`path ${folderSlug}`);
      const page = await mcp.call('create_team_page', {
        team: TEAM,
        kind: 'page',
        title: 'Restart the worker',
        parent: folderSlug,
        content: '# Restart the worker\n',
      });
      expect(page).toContain(`path ${folderSlug}/restart-the-worker`);
      const read = await mcp.call('team_pages', { team: TEAM, page: `${folderSlug}/restart-the-worker` });
      const version = Number(/- version: (\d+)/.exec(read)?.[1]);
      const edited = await mcp.call('edit_team_page', {
        team: TEAM,
        page: `${folderSlug}/restart-the-worker`,
        content: '# Restart the worker\n\n1. Drain the queue.\n2. Restart.\n',
        version,
      });
      expect(edited).toContain(`version ${version + 1}`);
      const moved = await mcp.call('move_team_page', {
        team: TEAM,
        page: `${folderSlug}/restart-the-worker`,
        to_root: true,
      });
      expect(moved).toContain('path restart-the-worker');
      await mcp.call('delete_team_page', { team: TEAM, page: 'restart-the-worker' });
      await mcp.call('delete_team_page', { team: TEAM, page: folderSlug });
      const after = await mcp.call('team_pages', { team: TEAM });
      expect(after).not.toContain(folderSlug);
      return { folder: folderSlug, edited_to_version: version + 1, archived: 2 };
    });

    await step(bob, `gets the docs site of ${REPO} ready for its CI: turns it on and reads the base path the build must use (\`kairos pages\`)`, async () => {
      const cli = await bob.cli();
      const api = await alice.api();
      const before = await cli.json(['pages', 'base-path', '--repo', REPO]);
      // Teardown leaves the switch as the seed had it.
      if (!before.enabled) {
        ledger.add({
          kind: 'site-switch',
          label: `${REPO} site off`,
          delete: async () => { await api.patch(`/api/repositories/${REPO}`, { site_enabled: false }); },
        });
      }
      const on = await cli.ok(['pages', 'on', '--repo', REPO]);
      expect(on).toContain(`The site of ${REPO} is on.`);
      const basePath = (await cli.ok(['pages', 'base-path', '--repo', REPO])).trim();
      // The path names the organization, the owner team and the repository.
      expect(basePath).toMatch(new RegExp(`^/pages/[^/]+/${TEAM}/repos/${REPO}/$`));
      const after = await cli.json(['pages', 'base-path', '--repo', REPO]);
      expect(after.enabled).toBe(true);
      return { repository: REPO, site: 'on', base_path: basePath, was_on_before: Boolean(before.enabled) };
    });

    await step(bob, 'asks where he works, and the answer is his board', async () => {
      const mcp = await bob.mcp();
      const text = await mcp.call('my_boards');
      expect(text).toContain(`${TEAM}-delivery`);
      expect(text).toContain('Backlog');
      const cli = await alice.cli();
      const teams = await cli.json(['teams', 'list', '--limit', '100']);
      const slugs = (teams.items ?? teams).map((t: any) => t.slug);
      expect(slugs).toContain(TEAM);
      return {
        my_board: `${TEAM}-delivery`,
        teams_in_directory: slugs.length,
      };
    });
  },
);
