//! The theme registry: the one place theme names, palettes and the saved
//! selection are defined. Native (`--theme`, the `t` key, the config file) and
//! the browser (`<select>`, `localStorage`) both resolve through [`ThemeId`], so
//! an unknown name is rejected everywhere the same way instead of being
//! silently ignored.
//!
//! Switching a theme swaps `flow.theme`'s palette and nothing else — session,
//! model, camera, selection and playhead are untouched. Every colour the UI
//! draws resolves from that palette at render time.

use rataflow::Palette;
use ratatui::style::Color;

// The app namespace (config directory, web `localStorage` key) is deliberately
// not defined here: each frontend passes its own, so the registry stays portable.

/// A built-in theme. Add a variant here, in [`ALL`](Self::ALL), and in the three
/// `match`es below; the page's `html[data-theme]` CSS block must mirror its
/// palette (kept in sync by hand).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeId {
    /// The original zoetrope look: neutral dark with a gold accent.
    ZoetropeGold,
    /// Cool graphite with an electric-blue accent, after oh-my-pi's titanium
    /// palette (MIT; see `NOTICE` and `OMP-THEME-LICENSE`).
    Titanium,
    /// Light preset for bright rooms and projectors.
    Light,
}

impl Default for ThemeId {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl ThemeId {
    /// The theme used when nothing is selected or the saved selection is bad.
    pub const DEFAULT: Self = Self::ZoetropeGold;

    /// Every theme, in cycling order.
    pub const ALL: [Self; 3] = [Self::ZoetropeGold, Self::Titanium, Self::Light];

    /// Stable machine name: the CLI value, config value and web value.
    pub const fn id(self) -> &'static str {
        match self {
            Self::ZoetropeGold => "zoetrope-gold",
            Self::Titanium => "titanium",
            Self::Light => "light",
        }
    }

    /// Human name for menus and the help overlay.
    pub const fn label(self) -> &'static str {
        match self {
            Self::ZoetropeGold => "Zoetrope gold",
            Self::Titanium => "Titanium",
            Self::Light => "Light",
        }
    }

    /// The eight semantic colours. Mirrored in `web/src/pages/app.astro`.
    pub const fn palette(self) -> Palette {
        match self {
            Self::Titanium => Palette {
                canvas_bg: Color::Rgb(0x15, 0x18, 0x20),
                surface: Color::Rgb(0x0f, 0x12, 0x16),
                muted: Color::Rgb(0x6b, 0x72, 0x80),
                subtle: Color::Rgb(0x9c, 0xa3, 0xb0),
                accent: Color::Rgb(0x00, 0xb4, 0xff),
                text: Color::Rgb(0xe8, 0xec, 0xf4),
                success: Color::Rgb(0x00, 0xff, 0x88),
                error: Color::Rgb(0xff, 0x47, 0x57),
            },
            // rataflow's stock dark palette with the gold accent: the original
            // zoetrope look, kept as indexed colours so terminals map it as before.
            Self::ZoetropeGold => Palette {
                accent: Color::Indexed(178),
                ..Palette::DARK
            },
            // Text and accents all clear 4.5:1 on `surface`.
            Self::Light => Palette {
                canvas_bg: Color::Rgb(0xee, 0xf0, 0xf4),
                surface: Color::Rgb(0xff, 0xff, 0xff),
                muted: Color::Rgb(0x6b, 0x72, 0x80),
                subtle: Color::Rgb(0x4b, 0x55, 0x63),
                accent: Color::Rgb(0x00, 0x66, 0xcc),
                text: Color::Rgb(0x11, 0x18, 0x27),
                success: Color::Rgb(0x0a, 0x7a, 0x3c),
                error: Color::Rgb(0xc6, 0x28, 0x28),
            },
        }
    }

    /// `canvas_bg` as RGB, for hosts that paint outside ratatui (the browser's
    /// canvas padding strip). Must equal [`palette`](Self::palette)'s `canvas_bg`.
    pub const fn canvas_rgb(self) -> [u8; 3] {
        match self {
            Self::ZoetropeGold => [0x12, 0x12, 0x12], // xterm 233
            Self::Titanium => [0x15, 0x18, 0x20],
            Self::Light => [0xee, 0xf0, 0xf4],
        }
    }

    /// Resolve an exact registered id (canonical lowercase, no trimming).
    /// `None` for anything else — callers must report it.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.id() == name)
    }

    /// The following theme in [`ALL`](Self::ALL), wrapping; `back` goes the
    /// other way.
    pub fn next(self, back: bool) -> Self {
        let n = Self::ALL.len();
        let i = Self::ALL.iter().position(|t| *t == self).unwrap_or(0);
        Self::ALL[(i + if back { n - 1 } else { 1 }) % n]
    }

    /// `"zoetrope-gold, titanium, light"`, for error messages.
    pub fn known() -> String {
        Self::ALL.map(Self::id).join(", ")
    }
}

#[cfg(feature = "native")]
pub use saved::{config_path, load, save};

/// The saved selection: `<config dir>/<app_dir>/theme.json` = `{"theme":"<id>"}`,
/// with `app_dir` chosen by the caller. Never touches anything outside that file.
#[cfg(feature = "native")]
mod saved {
    use std::io::Read;
    use std::path::{Path, PathBuf};

    use super::ThemeId;

    /// `$XDG_CONFIG_HOME` (when absolute), else `%APPDATA%` on Windows, else
    /// `~/.config`. `None` when none is known — the selection then simply isn't
    /// persisted.
    pub fn config_path(app_dir: &str) -> Option<PathBuf> {
        let abs = |k: &str| {
            std::env::var_os(k)
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
        };
        let base = abs("XDG_CONFIG_HOME")
            .or_else(|| if cfg!(windows) { abs("APPDATA") } else { None })
            .or_else(|| {
                std::env::var_os("HOME")
                    .filter(|h| !h.is_empty())
                    .map(|h| PathBuf::from(h).join(".config"))
            })?;
        Some(base.join(app_dir).join("theme.json"))
    }

    /// The saved theme. Missing, unreadable, oversized, malformed or
    /// unknown-name files all read as `None` (→ the default), never an error.
    pub fn load(path: &Path) -> Option<ThemeId> {
        #[derive(serde::Deserialize)]
        struct Saved {
            theme: String,
        }
        let mut text = String::new();
        std::fs::File::open(path)
            .ok()?
            .take(4096)
            .read_to_string(&mut text)
            .ok()?;
        ThemeId::parse(&serde_json::from_str::<Saved>(&text).ok()?.theme)
    }

    /// Persist `theme`: keep every other key of an existing valid JSON object,
    /// set `theme`, and replace the file atomically (unique temp file created
    /// with `create_new`, then rename) so a crash or a concurrent writer never
    /// leaves a half-written selection.
    pub fn save(path: &Path, theme: ThemeId) -> std::io::Result<()> {
        use std::io::Write;
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);

        let dir = path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir)?;

        let mut obj = std::fs::File::open(path)
            .ok()
            .and_then(|f| {
                let mut text = String::new();
                f.take(1 << 20).read_to_string(&mut text).ok()?;
                serde_json::from_str::<serde_json::Value>(&text).ok()
            })
            .and_then(|v| match v {
                serde_json::Value::Object(m) => Some(m),
                _ => None,
            })
            .unwrap_or_default();
        obj.insert("theme".into(), theme.id().into());
        let body = serde_json::to_string(&obj).map_err(std::io::Error::other)? + "\n";

        // `create_new` never clobbers; a stale temp from a crashed run just
        // moves us to the next sequence number.
        let (tmp, mut f) = {
            let mut tries = 0;
            loop {
                let tmp = dir.join(format!(
                    ".theme.{}.{}.tmp",
                    std::process::id(),
                    SEQ.fetch_add(1, Ordering::Relaxed)
                ));
                match std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&tmp)
                {
                    Ok(f) => break (tmp, f),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && tries < 16 => {
                        tries += 1;
                    }
                    Err(e) => return Err(e),
                }
            }
        };
        let written = f.write_all(body.as_bytes()).and_then(|()| f.sync_all());
        drop(f);
        match written.and_then(|()| std::fs::rename(&tmp, path)) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_registered_ids_only() {
        for t in ThemeId::ALL {
            assert_eq!(ThemeId::parse(t.id()), Some(t));
        }
        assert_eq!(ThemeId::parse("Light"), None, "ids are exact lowercase");
        assert_eq!(ThemeId::parse(" light"), None);
        assert_eq!(ThemeId::parse("gold"), None);
        assert_eq!(ThemeId::parse(""), None);
        assert_eq!(ThemeId::default(), ThemeId::ZoetropeGold);
    }

    #[test]
    fn canvas_rgb_matches_palette() {
        for t in ThemeId::ALL {
            match t.palette().canvas_bg {
                Color::Rgb(r, g, b) => assert_eq!(t.canvas_rgb(), [r, g, b], "{}", t.id()),
                Color::Indexed(233) => assert_eq!(t.canvas_rgb(), [0x12; 3], "{}", t.id()),
                other => panic!("{}: unhandled canvas colour {other:?}", t.id()),
            }
        }
    }

    #[test]
    fn ids_are_unique_and_next_wraps_both_ways() {
        let mut ids: Vec<_> = ThemeId::ALL.map(ThemeId::id).to_vec();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), ThemeId::ALL.len());
        let mut t = ThemeId::DEFAULT;
        for _ in 0..ThemeId::ALL.len() {
            t = t.next(false);
        }
        assert_eq!(t, ThemeId::DEFAULT);
        assert_eq!(ThemeId::ZoetropeGold.next(true), ThemeId::Light);
    }

    #[cfg(feature = "native")]
    #[test]
    fn saved_selection_round_trips_and_bad_files_fall_back() {
        let dir = std::env::temp_dir().join(format!("zoe-theme-{}", std::process::id()));
        let path = dir.join("nested").join("theme.json");
        assert_eq!(load(&path), None, "missing file");
        save(&path, ThemeId::Light).unwrap();
        assert_eq!(load(&path), Some(ThemeId::Light));
        for bad in ["", "{", "[]", r#"{"theme":"nope"}"#, r#"{"theme":3}"#] {
            std::fs::write(&path, bad).unwrap();
            assert_eq!(load(&path), None, "{bad:?}");
        }
        // Saving over a corrupt file recovers it.
        save(&path, ThemeId::ZoetropeGold).unwrap();
        assert_eq!(load(&path), Some(ThemeId::ZoetropeGold));
        // Saving keeps unrelated keys other tools put in the file.
        std::fs::write(&path, r#"{"theme":"light","other":{"k":1}}"#).unwrap();
        save(&path, ThemeId::Titanium).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["theme"], "titanium");
        assert_eq!(v["other"]["k"], 1);
        // No temp files left behind.
        let leftovers = std::fs::read_dir(path.parent().unwrap()).unwrap().count();
        assert_eq!(leftovers, 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
