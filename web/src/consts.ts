// Single source of truth for outward-facing site constants.
//
// Everything that links to the repository (header social link, hero buttons,
// "View on GitHub" actions, raw asset links) reads REPO_URL from here.

export const REPO_URL = 'https://github.com/furkankly/zoetrope';

/** Raw file base, for linking straight at files (LICENSE, TODO.md, …). */
export const REPO_RAW = `${REPO_URL}/blob/main`;

export const SITE_TITLE = 'zoetrope';
export const SITE_TAGLINE =
  'A terminal UI that visualizes Claude Code and Codex agent sessions as a live flow graph.';
