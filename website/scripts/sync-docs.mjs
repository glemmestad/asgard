#!/usr/bin/env node
/**
 * Sync product docs into the marketing site at build time.
 *
 * Adapts docs/docs/*.md for Astro:
 *   - Docusaurus admonitions   :::tip Title   ->  :::tip[Title]   (remark-directive)
 *   - cross-doc links          ](./x.md#a)    ->  ](/docs/x#a)
 *   - em dashes removed        house style, code blocks left untouched
 *   - link fragments normalized so anchors match rehype-slug ids
 *
 * Source preference:
 *   1. the docs in THIS repo (../../docs/docs) — the normal in-repo build, and
 *   2. the public product repo over HTTP — fallback for a standalone checkout.
 *
 * Runs as `prebuild`/`predev`, so a deploy always ships the current docs with no
 * human in the loop. On any failure the committed snapshot in src/content/docs is
 * left untouched as a fallback.
 */
import { writeFileSync, mkdirSync, readdirSync, rmSync, existsSync, readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const HERE = dirname(fileURLToPath(import.meta.url));
const OUT = join(HERE, '..', 'src', 'content', 'docs');
const LOCAL = join(HERE, '..', '..', 'docs', 'docs');

const REPO = process.env.DOCS_REPO ?? 'glemmestad/asgard';
const REF = process.env.DOCS_REF ?? 'main';

function adapt(md) {
  const lines = md.split('\n');
  let inCode = false;
  const out = lines.map((line) => {
    if (/^\s*```/.test(line)) { inCode = !inCode; return line; }
    if (inCode) return line;
    let l = line.replace(/[ \t]*—[ \t]*/g, ', ').replace(/,\s+,/g, ',').replace(/[ \t]+$/g, '');
    l = l.replace(/^:::(note|tip|info|caution|warning|danger)[ \t]+(\S.*?)[ \t]*$/, ':::$1[$2]');
    return l;
  });
  let text = out.join('\n');
  text = text.replace(/([^\n])\n, /g, '$1, ');
  text = text
    .replace(/\]\(\.\/([\w-]+)\.md/g, '](/docs/$1')
    .replace(/\/docs\/intro\b/g, '/docs')
    .replace(/\(([^)]*#[^)]*)\)/g, (_m, href) => `(${href.replace(/(#[\w-]*?)-{2,}/g, '$1-')})`);
  return text;
}

function writeAll(files) {
  mkdirSync(OUT, { recursive: true });
  for (const f of readdirSync(OUT)) if (f.endsWith('.md')) rmSync(join(OUT, f));
  for (const { name, body } of files) writeFileSync(join(OUT, name), adapt(body));
}

async function fromRemote() {
  const headers = { 'User-Agent': 'asgard-build-site' };
  if (process.env.GITHUB_TOKEN) headers.Authorization = `Bearer ${process.env.GITHUB_TOKEN}`;
  const res = await fetch(`https://api.github.com/repos/${REPO}/contents/docs/docs?ref=${REF}`, { headers });
  if (!res.ok) throw new Error(`GitHub contents ${res.status}`);
  const entries = (await res.json()).filter((e) => e.type === 'file' && e.name.endsWith('.md'));
  const files = [];
  for (const e of entries) {
    const raw = await fetch(e.download_url, { headers });
    if (!raw.ok) throw new Error(`fetch ${e.name} ${raw.status}`);
    files.push({ name: e.name, body: await raw.text() });
  }
  return files;
}

async function main() {
  let files;
  if (existsSync(LOCAL)) {
    files = readdirSync(LOCAL)
      .filter((n) => n.endsWith('.md'))
      .map((n) => ({ name: n, body: readFileSync(join(LOCAL, n), 'utf8') }));
    console.log(`[sync-docs] using ${files.length} local docs from docs/docs`);
  } else {
    files = await fromRemote();
    console.log(`[sync-docs] fetched ${files.length} docs from ${REPO}@${REF}`);
  }
  if (files.length === 0) throw new Error('no markdown docs found');
  writeAll(files);
}

main().catch((err) => {
  console.warn(`[sync-docs] WARNING: ${err.message} — keeping existing snapshot in src/content/docs`);
});
