// @ts-check
import { defineConfig, fontProviders } from 'astro/config';
import starlight from '@astrojs/starlight';

import { REPO_URL, SITE_TITLE, SITE_TAGLINE } from './src/consts.ts';

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
      // Two locales: English at the root (no URL prefix) and Simplified Chinese
      // under /zh/. Starlight ships built-in UI translations for zh-CN (the
      // language switcher, search, prev/next labels), picked up from the
      // locale's `lang`. The landing pages (/ and /zh/) are standalone Astro
      // pages rendered from the same component — not docs routes.
      locales: {
        root: { label: 'English', lang: 'en' },
        zh: { label: '简体中文', lang: 'zh-CN' },
      },
      // One sidebar shared by both locales: Starlight prefixes each `link`
      // with the current locale itself (/guides/… → /zh/guides/…), and each
      // item's `translations` (keyed by the locale's lang, here 'zh-CN')
      // overrides its label. The lone exception is /app: it is a standalone
      // page that self-localizes, so /zh/app is a redirect to it.
      sidebar: [
        {
          label: 'Start here',
          translations: { 'zh-CN': '从这里开始' },
          items: [
            {
              label: 'What is zoetrope?',
              translations: { 'zh-CN': '什么是 zoetrope？' },
              link: '/',
            },
            {
              label: 'Install',
              translations: { 'zh-CN': '安装' },
              link: '/guides/install/',
            },
            {
              label: 'Usage & keys',
              translations: { 'zh-CN': '用法与按键' },
              link: '/guides/usage/',
            },
          ],
        },
        {
          label: 'Concepts',
          translations: { 'zh-CN': '概念' },
          items: [
            {
              label: 'Design & architecture',
              translations: { 'zh-CN': '设计与架构' },
              link: '/guides/design/',
            },
          ],
        },
        {
          label: 'Try it',
          translations: { 'zh-CN': '试一试' },
          items: [
            {
              label: 'Open the browser app ↗',
              translations: { 'zh-CN': '打开浏览器版 ↗' },
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
