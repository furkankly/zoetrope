// @ts-check
import { execFileSync } from 'node:child_process';
import { defineConfig, fontProviders } from 'astro/config';
import starlight from '@astrojs/starlight';
import sitemap from '@astrojs/sitemap';

import { REPO_URL, SITE_TITLE, SITE_TAGLINE } from './src/consts.ts';

// ── <lastmod>, from git rather than from the clock ───────────────────────────
//
// The sitemap shipped bare <loc> entries, which gives a crawler no reason to
// come back to a page it has already seen — the thing that matters after an
// edit it needs to notice.
//
// The date is the last commit that touched the page's own source, NOT the build
// time. `new Date()` at build time is the easy version and it is a lie: every
// page would claim to have changed on every deploy, and Google's guidance is
// that it uses lastmod when a site reports it consistently and accurately, so a
// sitemap that cries wolf on all five URLs is worth less than no lastmod.
//
// The shared head components count too: Seo/Schema/Analytics inline into every
// page's HTML, so editing one really does change what the crawler fetches.
const SHARED = ['src/components'];

const SOURCES = {
  '/': ['src/pages/index.astro', ...SHARED],
  '/app/': ['src/pages/app.astro', ...SHARED],
  '/guides/install/': ['src/content/docs/guides/install.md', ...SHARED],
  '/guides/usage/': ['src/content/docs/guides/usage.md', ...SHARED],
  '/guides/design/': ['src/content/docs/guides/design.md', ...SHARED],
};

// A shallow clone (actions/checkout's default fetch-depth: 1) has one commit,
// so every file would date to that commit and the answer would be the build
// time wearing a disguise. The workflow asks for full history; if something
// ever serves a shallow tree anyway, drop lastmod rather than emit a wrong one.
const shallow = (() => {
  try {
    return execFileSync('git', ['rev-parse', '--is-shallow-repository'], {
      cwd: import.meta.dirname,
      encoding: 'utf8',
    }).trim() === 'true';
  } catch {
    return true; // no git at all — same conclusion, no lastmod
  }
})();

if (shallow) {
  console.warn('[sitemap] shallow or missing git history — emitting no <lastmod>');
}

function lastmod(pathname) {
  const paths = SOURCES[pathname];
  if (!paths || shallow) return undefined;
  try {
    const iso = execFileSync(
      'git',
      ['log', '-1', '--format=%cI', '--', ...paths],
      { cwd: import.meta.dirname, encoding: 'utf8' },
    ).trim();
    return iso ? new Date(iso) : undefined;
  } catch {
    return undefined;
  }
}

// https://astro.build/config
export default defineConfig({
  // Canonical origin. Served at the root of its own subdomain, so no `base` is
  // needed and asset paths stay root-relative. `site` is what lets the sitemap
  // integration emit absolute URLs (without it, sitemap generation is skipped).
  site: 'https://zoetrope.furkankly.dev',
  //
  // Brand type, self-hosted at build time (no runtime font CDN): Space Mono for
  // the display voice (wordmark, headlines) and JetBrains Mono for UI/body and
  // the ASCII flow-graph on the landing (chosen for its solid box-drawing +
  // block-glyph coverage so `╭─┤├╮ ● ◌ ✓ ▓ █` render aligned). Exposed as CSS
  // variables the bespoke landing (`src/pages/index.astro`) and the brand CSS use.
  fonts: [
    {
      provider: fontProviders.google(),
      name: 'Space Mono',
      cssVariable: '--font-display',
      weights: [400, 700],
      styles: ['normal'],
      fallbacks: ['ui-monospace', 'SFMono-Regular', 'monospace'],
    },
    {
      provider: fontProviders.google(),
      name: 'JetBrains Mono',
      cssVariable: '--font-mono',
      weights: [400, 500, 700],
      styles: ['normal'],
      fallbacks: ['ui-monospace', 'SFMono-Regular', 'monospace'],
    },
  ],
  integrations: [
    // Registered explicitly so it can carry `serialize`. Starlight adds its own
    // copy of this integration only when one is not already present (see the
    // `allIntegrations.find` guard in its index.ts), so this replaces it rather
    // than racing it — which also means the i18n config Starlight would have
    // passed is this site's to set, and a single-locale site has none.
    sitemap({
      serialize(item) {
        const { pathname } = new URL(item.url);
        const mod = lastmod(pathname);
        if (mod) item.lastmod = mod.toISOString();
        return item;
      },
    }),
    starlight({
      title: SITE_TITLE,
      tagline: SITE_TAGLINE,
      description: SITE_TAGLINE,
      // src/assets/zoetrope.svg is a detailed flow-graph illustration — great
      // large in the hero, muddy as a tiny header glyph, which is why the header
      // went without a logo for a long time. src/assets/icon.svg is the other
      // thing: a mark drawn to hold at 16px, so it can sit beside the wordmark
      // rather than replace it (`replacesTitle: false` keeps the styled mono
      // title, see `.site-title` in zoetrope.css).
      //
      // Both this and public/favicon.svg are copies of assets/icon.svg at the
      // repo root, put here by `assets/build.sh sync` and compared by
      // `build.sh check` — Astro will not import from outside web/.
      logo: { src: './src/assets/icon.svg', replacesTitle: false },
      favicon: '/favicon.svg',
      social: [{ icon: 'github', label: 'GitHub', href: REPO_URL }],
      // The brand is a dark terminal; default to dark and keep a tidy light mode.
      customCss: ['./src/styles/zoetrope.css'],
      // The in-browser app lives at /app (a standalone Astro page, not a docs
      // route), so surface it as a top-level CTA in the sidebar.
      sidebar: [
        {
          label: 'Start here',
          items: [
            { label: 'What is zoetrope?', link: '/' },
            { label: 'Install', link: '/guides/install/' },
            { label: 'Usage & keys', link: '/guides/usage/' },
          ],
        },
        {
          label: 'Concepts',
          items: [{ label: 'Design & architecture', link: '/guides/design/' }],
        },
        {
          label: 'Try it',
          items: [
            {
              label: 'Open the browser app ↗',
              link: '/app',
              attrs: { target: '_self' },
              badge: { text: 'wasm', variant: 'tip' },
            },
          ],
        },
      ],
      components: {
        // Load the brand display font (Space Mono) on docs pages too, so their
        // headings match the landing. See `src/components/StarlightHead.astro`.
        Head: './src/components/StarlightHead.astro',
      },
    }),
  ],
});
