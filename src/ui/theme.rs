//! Astralyx UI design tokens (https://ui.astralyx.dev), ported to GPUI.
//!
//! Values are the hex tokens from the site's stylesheet (`:root` = light,
//! `.dark` = dark). The site also ships `lab()` versions of the same colors for
//! wide-gamut displays; GPUI renders sRGB, so the hex values are used.
//!
//! Not portable to GPUI: letter-spacing (`tracking-*`), squircle corners
//! (`corner-shape: squircle`) and runtime `color-mix()`. Radii use the site's
//! own non-squircle fallback, and press colors are precomputed `color-mix`
//! results (see `primary_active` / `secondary_active`).

// The full token set is ported so new UI can use it; not every token is used yet.
#![allow(dead_code)]

use gpui::{App, Global, Hsla, Pixels, WindowAppearance, px, rgb, rgba};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Light,
    Dark,
}

impl Mode {
    pub fn toggled(self) -> Self {
        match self {
            Mode::Light => Mode::Dark,
            Mode::Dark => Mode::Light,
        }
    }
}

impl From<WindowAppearance> for Mode {
    fn from(appearance: WindowAppearance) -> Self {
        match appearance {
            WindowAppearance::Light | WindowAppearance::VibrantLight => Mode::Light,
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Mode::Dark,
        }
    }
}

/// The accent families of one mode.
#[derive(Clone, Copy, Debug)]
pub struct Tints {
    pub destructive: Tint,
    pub blue: Tint,
    pub violet: Tint,
    pub cyan: Tint,
    pub green: Tint,
    pub amber: Tint,
    pub rose: Tint,
}

/// One accent family (`--blue`, `--blue-foreground`, `--blue-hover`, `--blue-soft`, ...).
#[derive(Clone, Copy, Debug)]
pub struct Tint {
    pub base: Hsla,
    pub foreground: Hsla,
    pub hover: Hsla,
    pub soft: Hsla,
    pub soft_foreground: Hsla,
    pub soft_hover: Hsla,
    pub soft_foreground_hover: Hsla,
}

#[derive(Clone, Debug)]
pub struct Theme {
    pub mode: Mode,

    pub background: Hsla,
    pub foreground: Hsla,
    pub card: Hsla,
    pub card_foreground: Hsla,
    pub card_header: Hsla,
    pub popover: Hsla,
    pub popover_foreground: Hsla,
    pub group: Hsla,
    pub sidebar: Hsla,
    pub sidebar_foreground: Hsla,

    pub primary: Hsla,
    pub primary_foreground: Hsla,
    pub primary_hover: Hsla,
    /// `color-mix(in oklab, var(--primary-hover), var(--press-shade) 14%)`
    pub primary_active: Hsla,

    pub secondary: Hsla,
    pub secondary_foreground: Hsla,
    pub secondary_hover: Hsla,
    pub secondary_foreground_hover: Hsla,
    /// `color-mix(in oklab, var(--secondary-hover), var(--press-shade) 10%)`
    pub secondary_active: Hsla,

    pub muted: Hsla,
    pub muted_foreground: Hsla,
    pub accent: Hsla,
    pub accent_foreground: Hsla,

    pub border: Hsla,
    pub border_active: Hsla,
    pub input: Hsla,
    pub ring: Hsla,

    /// `color-mix(in oklab, var(--destructive-hover), var(--press-shade) 14%)`
    pub destructive_active: Hsla,

    /// Accents for the current mode, for content on `background` / `card`.
    pub tints: Tints,
    /// Accents for content on the sidebar. `--sidebar` is near-black in both
    /// modes, so this is always the dark set.
    pub sidebar_tints: Tints,

    pub chart: [Hsla; 5],
}

impl Global for Theme {}

impl Theme {
    pub fn global(cx: &App) -> &Self {
        cx.global::<Self>()
    }

    pub fn for_mode(mode: Mode) -> Self {
        match mode {
            Mode::Light => Self::light(),
            Mode::Dark => Self::dark(),
        }
    }

    pub fn light() -> Self {
        Self {
            mode: Mode::Light,

            background: c(0xffffff),
            foreground: c(0x0a0a0a),
            card: c(0xffffff),
            card_foreground: c(0x0a0a0a),
            card_header: c(0xfbfbfb),
            popover: c(0xffffff),
            popover_foreground: c(0x0a0a0a),
            group: c(0xf5f5f5),
            sidebar: c(0x020202),
            sidebar_foreground: c(0xfafafa),

            primary: c(0x171717),
            primary_foreground: c(0xfafafa),
            primary_hover: c(0x333333),
            primary_active: c(0x282828),

            secondary: c(0xf5f5f5),
            secondary_foreground: c(0x171717),
            secondary_hover: c(0xe8e8e8),
            secondary_foreground_hover: c(0x020202),
            secondary_active: c(0xcacaca),

            muted: c(0xf5f5f5),
            muted_foreground: c(0x707070),
            accent: c(0xf5f5f5),
            accent_foreground: c(0x171717),

            border: c(0xe5e5e5),
            border_active: c(0x868686),
            input: c(0xe5e5e5),
            ring: c(0xa1a1a1),

            destructive_active: c(0xa10002),

            tints: Tints::light(),
            sidebar_tints: Tints::dark(),

            chart: [
                c(0xf05100),
                c(0x009588),
                c(0x104e64),
                c(0xfcbb00),
                c(0xf99c00),
            ],
        }
    }

    pub fn dark() -> Self {
        Self {
            mode: Mode::Dark,

            background: c(0x020202),
            foreground: c(0xfafafa),
            card: c(0x070707),
            card_foreground: c(0xfafafa),
            card_header: c(0x141414),
            popover: c(0x070707),
            popover_foreground: c(0xfafafa),
            group: c(0x1d1d1d),
            // The site doesn't override the sidebar in dark mode.
            sidebar: c(0x020202),
            sidebar_foreground: c(0xfafafa),

            primary: c(0xe5e5e5),
            primary_foreground: c(0x070707),
            primary_hover: c(0xc4c4c4),
            primary_active: c(0xcccccc),

            secondary: c(0x121212),
            secondary_foreground: c(0xfafafa),
            secondary_hover: c(0x1f1f1f),
            secondary_foreground_hover: c(0xffffff),
            secondary_active: c(0x323232),

            muted: c(0x121212),
            muted_foreground: c(0xa1a1a1),
            accent: c(0x161616),
            accent_foreground: c(0xfafafa),

            border: ca(0xffffff1f),
            border_active: ca(0xffffff66),
            input: ca(0xffffff26),
            ring: c(0x737373),

            destructive_active: c(0xff9c98),

            tints: Tints::dark(),
            sidebar_tints: Tints::dark(),

            chart: [
                c(0x1447e6),
                c(0x00bb7f),
                c(0xf99c00),
                c(0xac4bff),
                c(0xff2357),
            ],
        }
    }
}

impl Tints {
    pub fn light() -> Self {
        Self {
            destructive: tint(
                0xe40014, 0xfafafa, 0xc50003, 0xffe7e4, 0xae000c, 0xffd6d0, 0x8d0003,
            ),
            blue: tint(
                0x156cdd, 0xfafafa, 0x0057bf, 0xe2f0ff, 0x0850ab, 0xcee3ff, 0x003d8f,
            ),
            violet: tint(
                0x8143e6, 0xfafafa, 0x6c2dc9, 0xf0ebff, 0x6130b1, 0xe3dbff, 0x500b9e,
            ),
            cyan: tint(
                0x007b8d, 0xfafafa, 0x008193, 0xd1f6fd, 0x005f6d, 0xbcebf4, 0x004b58,
            ),
            green: tint(
                0x00843b, 0xfafafa, 0x017b37, 0xd8f9dd, 0x005e27, 0xc4eecb, 0x00481c,
            ),
            amber: tint(
                0xefa831, 0x2f1e02, 0xdc9400, 0xffebc8, 0x794900, 0xfedca5, 0x603800,
            ),
            rose: tint(
                0xd8254f, 0xfafafa, 0xc41945, 0xffe7e8, 0xab0037, 0xffd5d7, 0x8e0027,
            ),
        }
    }

    pub fn dark() -> Self {
        Self {
            destructive: soft_tint(0xff6568, 0x150a09, 0xff8b88, 0xffb8b4, 0xffd6d3),
            blue: soft_tint(0x3082f6, 0x080d16, 0x5c9eff, 0x9dc6ff, 0xc3e0ff),
            violet: soft_tint(0x9c6aff, 0x0e0c15, 0xae8aff, 0xcfbeff, 0xe4d9ff),
            cyan: soft_tint(0x39b7cb, 0x041012, 0x63ccde, 0x8dddeb, 0xb7f0fb),
            green: soft_tint(0x45b164, 0x071009, 0x65c67d, 0x95dfa4, 0xb7f3c2),
            amber: soft_tint(0xf5ae39, 0x201301, 0xffc570, 0xfad18a, 0xffe8be),
            rose: soft_tint(0xfa5570, 0x150a0a, 0xff808d, 0xffb7bb, 0xffd5d8),
        }
    }
}

/// Corner radii. Uses the site's fallback for browsers without squircle
/// support (`--radius: .875rem`), since GPUI only draws circular corners.
pub mod radius {
    use super::*;

    pub const BASE: Pixels = px(14.);
    pub const XS: Pixels = px(6.);
    pub const SM: Pixels = px(8.);
    pub const MD: Pixels = px(10.);
    pub const LG: Pixels = px(12.);
    pub const XL: Pixels = px(14.);
    pub const XL2: Pixels = px(18.);
    pub const XL3: Pixels = px(24.);

    pub const CONTROL_SM: Pixels = px(10.);
    pub const CONTROL_MD: Pixels = px(11.);
    pub const CONTROL_LG: Pixels = px(12.);
    pub const CONTROL_XL: Pixels = px(14.);

    pub const CHECK_XS: Pixels = px(2.);
    pub const CHECK_SM: Pixels = px(5.);
    pub const CHECK_MD: Pixels = px(6.);
    pub const CHECK_LG: Pixels = px(8.);

    pub const GROUP_SM: Pixels = px(36.);
    pub const GROUP_MD: Pixels = px(42.);
    pub const GROUP_LG: Pixels = px(48.);
}

/// Monospace font. The site's stack starts with `ui-monospace` (SF Mono on
/// macOS), which isn't addressable by name; Menlo is the next macOS entry.
#[cfg(target_os = "macos")]
pub const FONT_MONO: &str = "Menlo";
#[cfg(windows)]
pub const FONT_MONO: &str = "Consolas";
#[cfg(not(any(target_os = "macos", windows)))]
pub const FONT_MONO: &str = "DejaVu Sans Mono";

fn c(hex: u32) -> Hsla {
    rgb(hex).into()
}

fn ca(hex_rgba: u32) -> Hsla {
    rgba(hex_rgba).into()
}

fn tint(
    base: u32,
    foreground: u32,
    hover: u32,
    soft: u32,
    soft_foreground: u32,
    soft_hover: u32,
    soft_foreground_hover: u32,
) -> Tint {
    Tint {
        base: c(base),
        foreground: c(foreground),
        hover: c(hover),
        soft: c(soft),
        soft_foreground: c(soft_foreground),
        soft_hover: c(soft_hover),
        soft_foreground_hover: c(soft_foreground_hover),
    }
}

/// Dark-mode accents: the soft fills are the base color at 18% (`2e`) and
/// 30% (`4d`) alpha, exactly as in the site's `.dark` tokens.
fn soft_tint(
    base: u32,
    foreground: u32,
    hover: u32,
    soft_foreground: u32,
    soft_foreground_hover: u32,
) -> Tint {
    Tint {
        base: c(base),
        foreground: c(foreground),
        hover: c(hover),
        soft: ca(base << 8 | 0x2e),
        soft_foreground: c(soft_foreground),
        soft_hover: ca(base << 8 | 0x4d),
        soft_foreground_hover: c(soft_foreground_hover),
    }
}
