// The old mdBook book served each page at <path>.html; Starlight serves it at
// <path>/. This integration writes a small HTML page at each old address that
// sends the reader to the new one (KAIROS-T-0389). GitHub Pages is static, so
// a server redirect is not possible.
//
// Each target is relative ("install-with-helm/"), so the pages work under any
// base path. The script keeps the #anchor of the old link; the meta refresh is
// for a browser with no JavaScript.
import { existsSync } from 'node:fs';
import { mkdir, writeFile } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

// mdBook addresses with no page of the same path.
const EXTRA = {
	'introduction.html': './', // the first chapter is now the home page
	'print.html': './', // mdBook's one-page print view
};

function html(target) {
	const t = JSON.stringify(target);
	return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>This page moved</title>
<meta name="robots" content="noindex">
<script>location.replace(${t} + location.hash);</script>
<meta http-equiv="refresh" content="0; url=${target}">
<link rel="canonical" href="${target}">
</head>
<body>
<p>This page moved to <a href="${target}">${target}</a>.</p>
</body>
</html>
`;
}

export default function oldBookRedirects() {
	return {
		name: 'kairos-old-book-redirects',
		hooks: {
			'astro:build:done': async ({ dir, pages, logger }) => {
				const out = fileURLToPath(dir);
				const files = { ...EXTRA };
				for (const { pathname } of pages) {
					// "how-to/install-with-helm/" -> "how-to/install-with-helm.html"
					if (!pathname || !pathname.endsWith('/') || pathname === '404/') continue;
					const path = pathname.slice(0, -1);
					const last = path.split('/').pop();
					files[`${path}.html`] = `${last}/`;
				}
				for (const [file, target] of Object.entries(files)) {
					const path = join(out, file);
					// Never write over a page of the site.
					if (existsSync(path)) continue;
					await mkdir(dirname(path), { recursive: true });
					await writeFile(path, html(target));
				}
				logger.info(`${Object.keys(files).length} redirects from the old book addresses`);
			},
		},
	};
}
