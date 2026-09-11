// Rasterises the mark into the favicons: assets/favicon.ico, favicon-96.png and
// apple-touch-icon.png, all copied into web/public by assets/build.sh sync.
//
//   node web/scripts/favicon.mjs       (run from the repo root)
//
// WHY, since the site already shipped a perfectly good SVG favicon: Google
// Search does not support SVG. Its documented list is BMP, GIF, ICO, PNG,
// JPEG, PPM and TIFF, and the site declared `/favicon.svg` and nothing else,
// with no `/favicon.ico` at the root to fall back to — so search results had no
// icon Google could read and showed the default globe instead. Browsers keep
// preferring the SVG (it stays first in the <link> list, and it is the only one
// that stays sharp at any size); these exist for the crawlers and for iOS.
//
// All of them are rendered FROM assets/icon.svg rather than drawn separately,
// for the reason the sync step gives: a second hand-maintained copy of a mark
// ends up a version behind the one it was drawn from.

import { readFileSync, writeFileSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { Resvg } from "@resvg/resvg-js";
import sharp from "sharp";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = join(HERE, "..", "..");

// The mark's own ground (the <rect> in icon.svg, and --ink in the pages).
const INK = "#121212";

const svg = readFileSync(join(REPO, "assets", "icon.svg"), "utf8");

/** The mark at `px` square, as PNG bytes. */
function render(px) {
  return new Resvg(svg, { fitTo: { mode: "width", value: px } }).render().asPng();
}

// ── favicon.ico ───────────────────────────────────────────────────────────────
// Three sizes in one file: 16 and 32 are what a browser tab and a bookmark bar
// actually ask for, and 48 is the size Google rasterises from. Modern .ico
// stores each entry as a whole PNG, which is why these go in verbatim — the
// BMP/AND-mask encoding the format started with is not needed by anything that
// has shipped this decade.
const ICO_SIZES = [16, 32, 48];

function ico(pngs) {
  const HEADER = 6;
  const ENTRY = 16;
  const header = Buffer.alloc(HEADER);
  header.writeUInt16LE(0, 0); // reserved
  header.writeUInt16LE(1, 2); // 1 = icon (2 would be a cursor)
  header.writeUInt16LE(pngs.length, 4);

  let offset = HEADER + ENTRY * pngs.length;
  const entries = pngs.map(({ size, png }) => {
    const e = Buffer.alloc(ENTRY);
    // 0 means 256 in this field; nothing here is that big, but the rule is the
    // reason the field is a single byte and worth not tripping over later.
    e.writeUInt8(size === 256 ? 0 : size, 0);
    e.writeUInt8(size === 256 ? 0 : size, 1);
    e.writeUInt8(0, 2); // palette size, 0 for truecolour
    e.writeUInt8(0, 3); // reserved
    e.writeUInt16LE(1, 4); // colour planes
    e.writeUInt16LE(32, 6); // bits per pixel
    e.writeUInt32LE(png.length, 8);
    e.writeUInt32LE(offset, 12);
    offset += png.length;
    return e;
  });

  return Buffer.concat([header, ...entries, ...pngs.map((p) => p.png)]);
}

const icoBuf = ico(ICO_SIZES.map((size) => ({ size, png: render(size) })));
writeFileSync(join(REPO, "assets", "favicon.ico"), icoBuf);

// ── favicon-96.png ────────────────────────────────────────────────────────────
// Google asks for a square of at least 8x8 but renders it on surfaces well
// above 48, so it is handed something bigger than the .ico's largest entry to
// downscale from rather than an icon it has to blow up.
const png96 = render(96);
writeFileSync(join(REPO, "assets", "favicon-96.png"), png96);

// ── apple-touch-icon.png ──────────────────────────────────────────────────────
// 180x180 is the iPhone home-screen size. Flattened onto the ink: iOS applies
// its own corner mask, and the mark's rounded <rect> leaves the four corners of
// the canvas transparent, which iOS composites against black — a dark ring
// around a dark icon. Filling them with the mark's own ground makes the seam
// disappear instead of merely moving it.
const apple = await sharp(render(180)).flatten({ background: INK }).png().toBuffer();
writeFileSync(join(REPO, "assets", "apple-touch-icon.png"), apple);

console.log(
  `favicon.ico ${ICO_SIZES.join("/")} (${icoBuf.length} bytes), ` +
    `favicon-96.png (${png96.length} bytes), ` +
    `apple-touch-icon.png 180 (${apple.length} bytes) -> assets/`,
);
