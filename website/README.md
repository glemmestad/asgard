# asgard.build

Marketing site for Asgard. Astro, static output, deploys to Vercel's free tier.
This is Vercel-hosted marketing — **not** part of the Asgard product binary.

## Edit content (the common case)

Almost all copy lives in one file: **`src/data/site.ts`**

- `site` — name, tagline, links (GitHub, docs, contact email)
- `loop` — the four-move onboarding loop cards
- `pillars` — the six platform capabilities
- `mcpTools` — the MCP tool chips in the Agents section
- `openCore` — the OSS vs Enterprise feature lists
- `faqs` — the FAQ accordion

Change a value there and the page updates. Section layout/markup lives in
`src/components/*.astro`; the design tokens (colors, type, spacing) are in
`src/styles/global.css`.

## Develop

```sh
npm install
npm run dev       # http://localhost:4321
npm run build     # static output to dist/
npm run preview   # serve the built dist/
```

## Deploy

Pushing to the connected Git repo auto-deploys via Vercel. Manual deploy:

```sh
npx vercel deploy --prod
```

Production URL: https://asgard-build.vercel.app
Custom domain `asgard.build`: add it in the Vercel project's Domains tab and
point DNS at Vercel (see HANDOFF notes).

## Assets

- `public/og.png` — social card (1200×630). Regenerate from `/tmp/og-gen.html`
  if the headline changes.
- `public/favicon.svg` — the Bifröst mark.
