// The Kairos book (KAIROS-T-0389): Astro Starlight with the Colliery theme
// starlight-aurora, in the four Diátaxis sections (KAIROS-S-0008).
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import mermaid from 'astro-mermaid';
import starlightAurora from 'starlight-aurora';
import oldBookRedirects from './old-book-redirects.mjs';

// The Pages workflow sets DOCS_SITE and DOCS_BASE. Locally the site is at /.
// On a pull request the workflow sets them empty: an empty site is not a URL.
const site = process.env.DOCS_SITE || undefined;
const base = process.env.DOCS_BASE || '/';

const PROJECT = 'Kairos';
const REPO = 'https://github.com/colliery-io/kairos';

// The sidebar is written out, not generated: it keeps the order and the short
// labels of the old SUMMARY.md. Add a new page here.
const page = (slug, label) => ({ slug, label });

export default defineConfig({
	site,
	base,
	integrations: [
		// Before Starlight. Light diagrams in the light theme; dark diagrams in
		// the dark theme.
		mermaid({ theme: 'dark', autoTheme: true }),
		starlight({
			title: PROJECT,
			description:
				'Kairos: Flight Levels work management. One stateless binary serving the GUI, REST API, MCP and SCIM, against a Postgres and an OIDC issuer you bring.',
			social: [{ icon: 'github', label: 'GitHub', href: REPO }],
			editLink: { baseUrl: `${REPO}/edit/main/docs/` },
			plugins: [starlightAurora()],
			sidebar: [
				{ label: 'Introduction', link: '/' },
				{
					label: 'Tutorials',
					items: [
						page('tutorials/run-kairos-locally', 'Run Kairos locally'),
						page('tutorials/deploy-to-kubernetes', 'Deploy Kairos to Kubernetes'),
					],
				},
				{
					label: 'How-to guides',
					items: [
						{
							label: 'For operators',
							items: [
								page('how-to/install-with-helm', 'Install with Helm'),
								page('how-to/configure-an-oidc-issuer', 'Configure an OIDC issuer'),
								page('how-to/use-local-accounts', 'Use local accounts'),
								page('how-to/serve-on-a-tailnet', 'Serve Kairos on a tailnet'),
								page('how-to/provision-a-tenant', 'Provision a tenant'),
								page('how-to/connect-a-git-forge', 'Connect a git forge'),
								page('how-to/read-a-private-repository', 'Read a private repository'),
								page('how-to/turn-on-github-write-back', 'Turn on GitHub write-back'),
								page('how-to/build-the-code-index-again', 'Build the code index again'),
								page(
									'how-to/use-a-hosted-model-for-the-code-index',
									'Use a hosted model for the code index summaries',
								),
								page('how-to/back-up-and-restore', 'Back up and restore'),
								page('how-to/rotate-keys', 'Rotate keys'),
								page('how-to/export-traces', 'Export traces to a collector'),
								page('how-to/provision-users-with-scim', 'Provision users with SCIM'),
							],
						},
						{
							label: 'For people doing the work',
							items: [
								page('how-to/set-up-a-board', 'Set up a board'),
								page('how-to/move-work-between-boards', 'Move work between boards'),
								page('how-to/wind-down-a-team', 'Wind down a team'),
								page('how-to/find-archived-work', 'Find archived work'),
								page('how-to/read-your-notifications', 'Read your notifications'),
							],
						},
						{
							label: 'For agent authors',
							items: [
								page('how-to/give-an-agent-your-key', 'Give an agent your key'),
								page('how-to/give-an-agent-machine-access', 'Give an agent machine access'),
								page('how-to/run-an-admin-agent', 'Run an admin agent'),
								page('how-to/connect-over-mcp', 'Connect over MCP'),
								page('how-to/read-notifications-as-an-agent', 'Read notifications as an agent'),
								page('how-to/dispatch-tasks-to-an-agent', 'Dispatch tasks to an agent runner'),
								page('how-to/configure-retrieval', 'Configure semantic retrieval'),
							],
						},
					],
				},
				{
					label: 'Reference',
					items: [
						page('reference/cli', 'CLI'),
						page('reference/configuration', 'Configuration'),
						page('reference/mcp-tools', 'MCP tools'),
						{
							label: 'REST API',
							items: [
								page('reference/rest-api', 'REST API'),
								page('reference/rest/work-items', 'Work items'),
								page('reference/rest/across-any-work-item', 'Across any work item'),
								page('reference/rest/boards-and-teams', 'Boards and teams'),
								page('reference/rest/execution-scope', 'Execution scope'),
								page('reference/rest/tenant-configuration', 'Tenant configuration'),
								page('reference/rest/machine-access', 'Machine access'),
								page('reference/rest/the-deployment-itself', 'The deployment itself'),
								page('reference/rest/signing-in', 'Signing in'),
								page('reference/rest/schemas', 'Schemas'),
							],
						},
						page('reference/events', 'Events'),
						page('reference/webhooks', 'Webhook deliveries'),
						page('reference/scim', 'SCIM'),
						page('reference/capabilities', 'Capabilities'),
						page('reference/short-codes', 'Short codes'),
						page('reference/errors', 'Errors'),
						page('reference/glossary', 'Glossary'),
					],
				},
				{
					label: 'Explanation',
					items: [
						page('explanation/flight-levels', 'Flight levels'),
						page('explanation/teams-and-boards', 'Teams and boards'),
						page('explanation/capabilities-and-access', 'Capabilities and access'),
						page('explanation/choosing-how-people-log-in', 'Choosing how people log in'),
						page('explanation/archiving', 'Archiving'),
						page(
							'explanation/repositories-as-execution-scope',
							'Repositories as execution scope',
						),
						page('explanation/finding-related-work', 'Finding related work'),
						page('explanation/job-queue', 'The job queue'),
					],
				},
			],
		}),
		// The old mdBook addresses (how-to/x.html) go to the new pages.
		oldBookRedirects(),
	],
});
