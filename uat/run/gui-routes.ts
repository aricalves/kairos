// The GUI side of the drift gate (KAIROS-T-0586).
//
// The gate asks the deployment for its MCP tools and CLI nouns. A GUI has
// no such list on the wire, so the routes come from the route map of the
// app, `crates/kairos-web/src/app.rs`, read when the gate runs: a page
// added to the app shows up in the gate the day it lands.
//
// What the journeys opened is measured at run time, as for MCP and the
// CLI: `Persona.gui()` records the path of each navigation of its page.
// What the e2e tier opens is read from its specs (the `goto` paths), so
// the gate can name, for each route that no journey opens, the specs that
// do — or that none does.
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const REPO = path.join(HERE, '..', '..');
export const ROUTE_MAP = path.join(REPO, 'crates', 'kairos-web', 'src', 'app.rs');
export const E2E_SPECS = path.join(REPO, 'e2e', 'tests');

/**
 * The route patterns of a Leptos route map (`/boards/:board`, `/teams/:slug/pages/*path`),
 * with the paths of the parent routes put in front. Pure.
 */
export function routePatterns(source: string): string[] {
  const routes: string[] = [];
  const parents: string[] = [];
  const tag = /<(\/?)(ParentRoute|Route)\b(?:[^>]*?path=path!\("([^"]*)"\))?[^>]*?(\/?)>/g;
  for (const match of source.matchAll(tag)) {
    const [, closing, kind, segment, selfClosing] = match;
    if (closing) {
      if (kind === 'ParentRoute') parents.pop();
      continue;
    }
    const part = (segment ?? '').replace(/^\/+|\/+$/g, '');
    const full = [...parents, part].filter(Boolean).join('/');
    if (kind === 'ParentRoute' && !selfClosing) {
      parents.push(part);
      continue;
    }
    routes.push(`/${full}`);
  }
  return [...new Set(routes)].sort();
}

/** Does a path (no query) belong to a route pattern? Pure. */
export function matchesRoute(pattern: string, pathname: string): boolean {
  const want = pattern.split('/').filter(Boolean);
  const have = pathname.split('/').filter(Boolean);
  for (let i = 0; i < want.length; i += 1) {
    if (want[i].startsWith('*')) return true;
    if (i >= have.length) return false;
    if (want[i].startsWith(':')) continue;
    if (want[i] !== decodeURIComponent(have[i])) return false;
  }
  return want.length === have.length;
}

/** The most specific pattern of a path: the one with the most fixed segments. */
export function routeOf(patterns: string[], pathname: string): string | undefined {
  const fixed = (p: string) => p.split('/').filter((s) => s && !/^[:*]/.test(s)).length;
  return patterns
    .filter((p) => matchesRoute(p, pathname))
    .sort((a, b) => fixed(b) - fixed(a) || b.length - a.length)[0];
}

/**
 * The paths that a spec opens with `goto(...)`. A `${...}` part of a template
 * is one segment; the query is dropped. Pure.
 */
export function gotoPaths(source: string): string[] {
  const out: string[] = [];
  for (const match of source.matchAll(/goto\(\s*(['`])(\/[^'`]*)\1/g)) {
    const raw = match[2].replace(/\$\{[^}]*\}/g, 'x');
    out.push(raw.split(/[?#]/)[0] || '/');
  }
  return out;
}

export interface GuiCoverage {
  /** Each route of the app. */
  routes: string[];
  /** The routes that a journey opened. */
  opened: string[];
  /** Each route that no journey opened, with the e2e specs that open it ("none" when no spec does). */
  outside: string[];
}

/** The GUI coverage of a run, from the paths that the journeys opened. */
export function guiCoverage(openedPaths: string[]): GuiCoverage {
  const routes = routePatterns(fs.readFileSync(ROUTE_MAP, 'utf8'));
  const opened = new Set<string>();
  for (const p of openedPaths) {
    const route = routeOf(routes, p);
    if (route) opened.add(route);
  }
  const specsOf = new Map<string, Set<string>>();
  for (const file of fs.readdirSync(E2E_SPECS).filter((f) => f.endsWith('.spec.ts')).sort()) {
    for (const p of gotoPaths(fs.readFileSync(path.join(E2E_SPECS, file), 'utf8'))) {
      const route = routeOf(routes, p);
      if (!route) continue;
      if (!specsOf.has(route)) specsOf.set(route, new Set());
      specsOf.get(route)!.add(file);
    }
  }
  const outside = routes
    .filter((r) => !opened.has(r))
    .map((r) => {
      const specs = [...(specsOf.get(r) ?? [])];
      if (specs.length === 0) return `gui:${r} — none: no journey and no e2e spec opens it`;
      const more = specs.length > 3 ? ` and ${specs.length - 3} more` : '';
      return `gui:${r} — e2e: ${specs.slice(0, 3).join(', ')}${more}`;
    });
  return { routes, opened: [...opened].sort(), outside };
}
