// Renders the social card: assets/og.png, copied to web/public/og.png.
//
//   node web/scripts/og.mjs            (run from the repo root)
//
// Why a rendered card rather than the raw screenshot it replaced: a card is
// viewed at roughly 500px wide in a feed, and at that size a 1200x630 shot of a
// terminal is a grey blur. Every label in it was unreadable, and the word
// "zoetrope" appeared nowhere on it — the example's sidebar says "Overview".
// So the name and the one-line pitch are set at a size that survives, and the
// real UI appears as a CROP rather than the whole dense frame.
//
// The picture is assets/og-shot.png, written directly by assets/og.tape, so
// the picture on the card is the actual library and one command regenerates the
// whole thing. See docs/DEMO-ASSETS.md.
//
// satori lays out flexbox and emits SVG; resvg rasterises it. Satori rules that
// bite: every element with more than one child needs an explicit `display:flex`,
// there is no line-clamp, and images must be data URIs (no network, no paths).

import { readFileSync, writeFileSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import satori from "satori";
import { Resvg } from "@resvg/resvg-js";
import sharp from "sharp";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = join(HERE, "..", "..");

const W = 1200;
const H = 630;

// The app's own palette (the :root block in web/src/pages/app.astro) so the
// card and the page it links to look like one thing.
const INK = "#121212";
const LINE = "#2a2a2a";
const GOLD = "#d7af00";
const GOLD_BRIGHT = "#f0d56a";
const DIM = "#585858";
const TEXT = "#e6e6e6";

const font = (f) => readFileSync(join(REPO, "assets", "fonts", f));

// The screenshot inset, cut straight out of the tape recording.
//
// The WHOLE frame. Cropping was the first attempt and it cost too much: a
// 1160x330 window on the graph dropped the prompt timeline and the status bar
// outright — two of the things that make zoetrope legible as a tool.
//
// So the frame is the background and the words sit on top of a scrim. The
// screenshot no longer has to carry the message on its own, which is what made
// legibility fight coverage in the first place.
const CROP = null;

// A true-colour PNG, not a GIF frame. The previous version pulled the last page
// out of an animated GIF, so the card was assembled from 255 quantised colours;
// the screenshot carries roughly ten thousand.
async function shotDataUri() {
  const shot = join(REPO, "assets", "og-shot.png");
  let img = sharp(shot);
  if (CROP) img = img.extract(CROP);
  // Lift it. A terminal screenshot is nearly black by nature, and under a scrim
  // it went flat and sooty — the gold of the timeline markers and the live dot
  // on the root node were doing nothing. Brightness alone greys it out, so
  // saturation comes up with it and the colours stay colours.
  img = img.modulate({ brightness: 1.22, saturation: 1.35 });
  const png = await img.png().toBuffer();
  return `data:image/png;base64,${png.toString("base64")}`;
}

const shot = await shotDataUri();

// The mark, rasterised here rather than embedded as SVG. Satori will take an
// SVG data URI but rasterises it at its own scale, and this one is mostly a
// dashed 3.5px ring that goes to mush when that guess comes in low; rendering
// it at 3x and letting satori scale a bitmap down keeps the slits separate.
function markDataUri(px) {
  const svg = readFileSync(join(REPO, "assets", "icon.svg"), "utf8");
  const png = new Resvg(svg, { fitTo: { mode: "width", value: px * 3 } })
    .render()
    .asPng();
  return `data:image/png;base64,${png.toString("base64")}`;
}

const MARK_PX = 46;
const mark = markDataUri(MARK_PX);

const card = {
  type: "div",
  props: {
    style: {
      width: "100%",
      height: "100%",
      display: "flex",
      position: "relative",
      backgroundColor: INK,
      fontFamily: "JetBrains Mono",
    },
    children: [
      // The real UI, full bleed.
      {
        type: "img",
        props: {
          src: shot,
          style: { position: "absolute", left: 0, top: 0, width: W, height: H },
        },
      },
      // Scrim: opaque on the RIGHT, where the words go, clearing toward the
      // left. That keeps both things worth seeing: the fan-out from the root
      // session node, and the prompt timeline along the bottom.
      {
        type: "div",
        props: {
          style: {
            position: "absolute",
            left: 0,
            top: 0,
            width: W,
            height: H,
            backgroundImage: `linear-gradient(260deg, ${INK} 0%, ${INK} 30%, rgba(18,18,18,0.86) 44%, rgba(18,18,18,0.04) 100%)`,
          },
        },
      },
      // Full-width accent rule. TOP edge here, not bottom: this app's status
      // bar (with the wordmark in it) runs along the very bottom of the frame,
      // and a bottom rule sliced it in half. rataflow's frame is empty down
      // there, so its rule stays at the bottom.
      {
        type: "div",
        props: {
          style: {
            position: "absolute",
            left: 0,
            top: 0,
            width: W,
            height: 12,
            backgroundColor: GOLD,
          },
        },
      },
      {
        type: "div",
        props: {
          style: {
            position: "relative",
            display: "flex",
            flexDirection: "column",
            justifyContent: "center",
            alignItems: "flex-end",
            marginLeft: "auto",
            width: 690,
            height: H,
            padding: "0 56px",
            textAlign: "right",
          },
          children: [
            {
              type: "div",
              props: {
                style: {
                  display: "flex",
                  alignItems: "center",
                  gap: 16,
                  marginBottom: 26,
                },
                children: [
                  {
                    type: "img",
                    props: {
                      src: mark,
                      width: MARK_PX,
                      height: MARK_PX,
                      style: { display: "flex" },
                    },
                  },
                  {
                    type: "div",
                    props: {
                      style: { fontSize: 30, color: GOLD_BRIGHT, fontWeight: 700 },
                      children: "zoetrope",
                    },
                  },
                  {
                    type: "div",
                    props: {
                      style: {
                        display: "flex",
                        flex: 1,
                        height: 2,
                        backgroundColor: LINE,
                      },
                    },
                  },
                  {
                    type: "div",
                    props: {
                      style: { fontSize: 17, color: DIM },
                      children: "a terminal app",
                    },
                  },
                ],
              },
            },
            {
              type: "div",
              props: {
                style: { fontSize: 50, color: TEXT, fontWeight: 700, lineHeight: 1.16 },
                children: "Watch",
              },
            },
            // The accent line is INVERTED rather than merely coloured: dark
            // text on a solid block, the way a terminal draws a selection. A
            // feed is mostly light cards, so a dark card needs one saturated
            // shape to hold a thumbnail's worth of attention. Coloured text
            // alone disappeared at that size.
            {
              type: "div",
              props: {
                style: {
                  display: "flex",
                  alignSelf: "flex-end",
                  backgroundColor: GOLD,
                  color: INK,
                  fontSize: 50,
                  fontWeight: 700,
                  lineHeight: 1.16,
                  padding: "4px 16px",
                  marginTop: 6,
                },
                children: "your agents work",
              },
            },
            // Two lines by hand rather than by wrapping: the column is right
            // aligned, so an automatic break leaves one orphaned word hanging
            // off the end of the card.
            {
              type: "div",
              props: {
                style: {
                  display: "flex",
                  flexDirection: "column",
                  alignItems: "flex-end",
                  fontSize: 19,
                  color: DIM,
                  marginTop: 22,
                  lineHeight: 1.5,
                },
                children: [
                  { type: "div", props: { children: "claude code and codex sessions" } },
                  { type: "div", props: { children: "as a live flow graph" } },
                ],
              },
            },
          ],
        },
      },
    ],
  },
};

const svg = await satori(card, {
  width: W,
  height: H,
  fonts: [
    { name: "JetBrains Mono", data: font("JetBrainsMono-Regular.ttf"), weight: 400, style: "normal" },
    { name: "JetBrains Mono", data: font("JetBrainsMono-Bold.ttf"), weight: 700, style: "normal" },
  ],
});

const png = new Resvg(svg, { fitTo: { mode: "width", value: W } })
  .render()
  .asPng();

writeFileSync(join(REPO, "assets", "og.png"), png);
writeFileSync(join(REPO, "web", "public", "og.png"), png);
console.log(`og.png ${W}x${H} (${png.length} bytes) -> assets/ and web/public/`);
