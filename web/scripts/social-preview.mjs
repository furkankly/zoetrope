// Renders assets/social-preview.png, the image GitHub shows when the repo is
// linked anywhere (Settings -> Social preview, 1280x640).
//
//   node web/scripts/social-preview.mjs      (run from the repo root)
//
// Why this is NOT og.png. The two cards are linked from different places and
// answer different questions. og.png sells the app to someone who already
// clicked a link about it, so it leads with a screenshot of a real session.
// This one is what a stranger sees next to a bare repo name in a feed, so it
// leads with the mark: at 2:1 the drum's landscape card is exactly the right
// shape, and the slits and the gold arc say "replay" before the name is read.
//
// Two-pass, for the same reason og.mjs rasterises the mark: resvg does not
// composite one SVG inside another, so the mark is rendered to a PNG first at
// 2x and embedded. Text is drawn by resvg using the repo's own JetBrains Mono
// rather than whatever the machine happens to have installed.

import { readFileSync, writeFileSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { Resvg } from "@resvg/resvg-js";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = join(HERE, "..", "..");

const W = 1280;
const H = 640;

// The app's own palette, same as og.mjs.
const INK = "#121212";
const GOLD = "#d7af00";
const GOLD_BRIGHT = "#f0d56a";
const DIM = "#585858";
const TEXT = "#e6e6e6";

const FONTS = [
  join(REPO, "assets", "fonts", "JetBrainsMono-Regular.ttf"),
  join(REPO, "assets", "fonts", "JetBrainsMono-Bold.ttf"),
];

// Laid out on one axis: mark from MARK_X, type from TEXT_X, and the dot field
// fades across the gutter between them so both numbers stay honest.
const MARK_X = 70;
const MARK_W = 560;
const TEXT_X = 690;

// The mark, at 2x the box it lands in so it stays sharp.
const markPng = new Resvg(readFileSync(join(REPO, "assets", "mark.svg"), "utf8"), {
  fitTo: { mode: "width", value: MARK_W * 2 },
}).render();
const markUri = `data:image/png;base64,${markPng.asPng().toString("base64")}`;
const markH = Math.round((markPng.height / markPng.width) * MARK_W);

// The mark brings its own ink card and dot grid (20px on a 220-wide canvas),
// and it is opaque, so it punches a hole in whatever it sits on. Matching the
// canvas grid to the mark's *scaled* pitch is what makes that hole invisible:
// one continuous field, with the drum floating on it rather than sitting in a
// panel whose edge you can find.
const DOT = (20 * MARK_W) / 220;

// The right-hand column is hand-wrapped: there is no line box here to wrap
// against, and two lines at this size is a layout decision rather than a
// measurement.
const card = `<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="${W}" height="${H}" viewBox="0 0 ${W} ${H}">
  <defs>
    <pattern id="dots" width="${DOT}" height="${DOT}" patternUnits="userSpaceOnUse">
      <circle cx="${DOT / 2}" cy="${DOT / 2}" r="${(1.2 * MARK_W) / 220}" fill="${DIM}"/>
    </pattern>
    <!-- Matching the mark's pitch makes the dots large at this size, which is
         fine behind a drawing and not behind 27px text. The field fades out
         across the gutter so the right column sits on flat ink. -->
    <linearGradient id="scrim" x1="0" x2="1" y1="0" y2="0">
      <stop offset="${(MARK_X + MARK_W) / W}" stop-color="${INK}" stop-opacity="0"/>
      <stop offset="${TEXT_X / W}" stop-color="${INK}" stop-opacity="1"/>
    </linearGradient>
  </defs>
  <rect width="${W}" height="${H}" fill="${INK}"/>
  <rect width="${W}" height="${H}" fill="url(#dots)" opacity="0.4"/>
  <rect width="${W}" height="${H}" fill="url(#scrim)"/>

  <image x="${MARK_X}" y="${Math.round((H - markH) / 2)}" width="${MARK_W}" height="${markH}" xlink:href="${markUri}"/>

  <g font-family="JetBrains Mono">
    <text x="${TEXT_X}" y="268" font-size="72" font-weight="700" fill="${GOLD_BRIGHT}">zoetrope</text>
    <text x="${TEXT_X}" y="332" font-size="27" fill="${TEXT}">Watch Claude Code and Codex</text>
    <text x="${TEXT_X}" y="370" font-size="27" fill="${TEXT}">sessions as a live flow graph.</text>
    <rect x="${TEXT_X}" y="404" width="150" height="2" fill="${DIM}"/>
    <text x="${TEXT_X}" y="452" font-size="23" fill="${DIM}">a terminal app · zoetrope.furkankly.dev</text>
  </g>

  <!-- Accent rule on the TOP edge, matching this repo's og.png rather than
       rataflow's. Same reasoning kept consistent within the brand. -->
  <rect x="0" y="0" width="${W}" height="12" fill="${GOLD}"/>
</svg>`;

const png = new Resvg(card, {
  fitTo: { mode: "width", value: W },
  font: { fontFiles: FONTS, loadSystemFonts: false, defaultFontFamily: "JetBrains Mono" },
})
  .render()
  .asPng();

const out = join(REPO, "assets", "social-preview.png");
writeFileSync(out, png);
console.log(`social-preview.png ${W}x${H} (${png.length} bytes) -> assets/`);
