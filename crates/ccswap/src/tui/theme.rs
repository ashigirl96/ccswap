//! The cswap dark and light palettes (upstream `tui/theme.py`).

use ratatui::style::Color;

pub const WARN_PCT: f64 = 70.0;
pub const CRIT_PCT: f64 = 90.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub accent: Color,
    pub foreground: Color,
    pub muted: Color,
    pub background: Color,
    pub surface: Color,
    pub panel: Color,
    pub sev_ok: Color,
    pub sev_warn: Color,
    pub sev_crit: Color,
    pub track: Color,
}

const fn hex(rgb: u32) -> Color {
    Color::Rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

pub const DARK: Palette = Palette {
    accent: hex(0xd7875f),
    foreground: hex(0xe8e4de),
    muted: hex(0x8a8a8a),
    background: hex(0x141414),
    surface: hex(0x1e1e1e),
    panel: hex(0x262626),
    sev_ok: hex(0x87af87),
    sev_warn: hex(0xd7af5f),
    sev_crit: hex(0xd75f5f),
    track: hex(0x3a3a3a),
};

pub const LIGHT: Palette = Palette {
    accent: hex(0x954c2a),
    foreground: hex(0x2b2723),
    muted: hex(0x635d55),
    background: hex(0xfaf7f2),
    surface: hex(0xefeae1),
    panel: hex(0xe2dbcf),
    sev_ok: hex(0x3d6b3d),
    sev_warn: hex(0x795911),
    sev_crit: hex(0xad3128),
    track: hex(0xcec7ba),
};

impl Palette {
    pub fn severity(&self, pct: Option<f64>) -> Color {
        match pct {
            None => self.muted,
            Some(p) if p >= CRIT_PCT => self.sev_crit,
            Some(p) if p >= WARN_PCT => self.sev_warn,
            Some(_) => self.sev_ok,
        }
    }
}

/// The `ui.theme` setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeName {
    Dark,
    Light,
    Auto,
}

impl ThemeName {
    pub const ALL: [ThemeName; 3] = [ThemeName::Dark, ThemeName::Light, ThemeName::Auto];

    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "dark" => ThemeName::Dark,
            "light" => ThemeName::Light,
            "auto" => ThemeName::Auto,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ThemeName::Dark => "dark",
            ThemeName::Light => "light",
            ThemeName::Auto => "auto",
        }
    }

    pub fn next(self) -> Self {
        match self {
            ThemeName::Dark => ThemeName::Light,
            ThemeName::Light => ThemeName::Auto,
            ThemeName::Auto => ThemeName::Dark,
        }
    }

    /// `auto` follows the detected background, dark when unknown.
    pub fn palette(self, detected_light: Option<bool>) -> Palette {
        match self {
            ThemeName::Light => LIGHT,
            ThemeName::Auto if detected_light == Some(true) => LIGHT,
            _ => DARK,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_ramp_edges() {
        assert_eq!(DARK.severity(None), DARK.muted);
        assert_eq!(DARK.severity(Some(69.9)), DARK.sev_ok);
        assert_eq!(DARK.severity(Some(70.0)), DARK.sev_warn);
        assert_eq!(DARK.severity(Some(89.9)), DARK.sev_warn);
        assert_eq!(DARK.severity(Some(90.0)), DARK.sev_crit);
    }

    /// upstream test_theme.py: the palettes are the published hex values.
    #[test]
    fn palettes_match_upstream_hex_values() {
        let hexes = |p: Palette| {
            [
                p.accent,
                p.foreground,
                p.muted,
                p.background,
                p.surface,
                p.panel,
                p.sev_ok,
                p.sev_warn,
                p.sev_crit,
                p.track,
            ]
            .map(|c| match c {
                Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
                other => panic!("{other:?}"),
            })
        };
        assert_eq!(
            hexes(DARK),
            [
                "#d7875f", "#e8e4de", "#8a8a8a", "#141414", "#1e1e1e", "#262626", "#87af87",
                "#d7af5f", "#d75f5f", "#3a3a3a"
            ]
        );
        assert_eq!(
            hexes(LIGHT),
            [
                "#954c2a", "#2b2723", "#635d55", "#faf7f2", "#efeae1", "#e2dbcf", "#3d6b3d",
                "#795911", "#ad3128", "#cec7ba"
            ]
        );
    }

    fn luminance(c: Color) -> f64 {
        let Color::Rgb(r, g, b) = c else {
            panic!("rgb expected")
        };
        let f = |v: u8| {
            let c = f64::from(v) / 255.0;
            if c <= 0.03928 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * f(r) + 0.7152 * f(g) + 0.0722 * f(b)
    }

    fn contrast(a: Color, b: Color) -> f64 {
        let (x, y) = (luminance(a), luminance(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    #[test]
    fn light_text_meets_aa_on_all_backgrounds() {
        let p = LIGHT;
        for color in [
            p.foreground,
            p.muted,
            p.accent,
            p.sev_ok,
            p.sev_warn,
            p.sev_crit,
        ] {
            for bg in [p.background, p.surface, p.panel] {
                assert!(contrast(color, bg) >= 4.5, "{color:?} on {bg:?}");
            }
        }
    }

    #[test]
    fn theme_cycle_and_resolution() {
        assert_eq!(ThemeName::Dark.next(), ThemeName::Light);
        assert_eq!(ThemeName::Light.next(), ThemeName::Auto);
        assert_eq!(ThemeName::Auto.next(), ThemeName::Dark);
        assert_eq!(ThemeName::Auto.palette(None), DARK);
        assert_eq!(ThemeName::Auto.palette(Some(true)), LIGHT);
        assert_eq!(ThemeName::parse("bogus"), None);
    }
}
