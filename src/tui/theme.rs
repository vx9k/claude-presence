//! Colors. ratatui passes colors through as given, so a terminal that can't
//! show RGB would get garbage: pick the richest mode the terminal advertises
//! and map one semantic theme onto it.

use ratatui::style::{Color, Modifier, Style};

/// How much color the terminal can show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorMode {
    /// `NO_COLOR`: modifiers (bold, dim, reversed) only.
    None,
    /// The 16 named ANSI colors.
    Ansi16,
    /// The xterm 256-color palette.
    Indexed,
    /// 24-bit RGB.
    TrueColor,
}

/// What a color means; every widget asks for one of these, never a color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Claude's orange: the app name, selection, highlights.
    Accent,
    /// Discord's blurple: everything about the card.
    Discord,
    /// Secondary text, borders, separators.
    Muted,
    Good,
    Warn,
    Bad,
    Info,
    /// The "thinking" status.
    Thinking,
    /// Text on a colored pill.
    OnPill,
    /// TOML strings in the config tab.
    Str,
    /// TOML numbers and booleans.
    Num,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub mode: ColorMode,
}

impl Palette {
    /// The palette for the terminal described by `env` (an environment
    /// lookup, `std::env::var` in practice).
    pub fn detect(env: impl Fn(&str) -> Option<String>) -> Palette {
        Palette::detect_on(env, cfg!(windows))
    }

    /// `detect` as on Windows (`windows`) or elsewhere: Windows Terminal
    /// (`WT_SESSION`) renders RGB but doesn't set `COLORTERM`.
    pub fn detect_on(env: impl Fn(&str) -> Option<String>, windows: bool) -> Palette {
        let set = |k: &str| env(k).filter(|v| !v.is_empty());
        let mode = if set("NO_COLOR").is_some() {
            ColorMode::None
        } else if set("COLORTERM")
            .is_some_and(|v| v.eq_ignore_ascii_case("truecolor") || v.eq_ignore_ascii_case("24bit"))
            || (windows && set("WT_SESSION").is_some())
        {
            ColorMode::TrueColor
        } else if set("TERM").is_some_and(|v| v.contains("256color")) {
            ColorMode::Indexed
        } else {
            ColorMode::Ansi16
        };
        Palette { mode }
    }

    /// `role`'s color in this mode; `None` without colors.
    pub fn color(&self, role: Role) -> Option<Color> {
        // (RGB, xterm index, named)
        let (rgb, idx, named) = match role {
            Role::Accent => ((217, 119, 87), 173, Some(Color::LightRed)),
            Role::Discord => ((88, 101, 242), 63, Some(Color::LightBlue)),
            Role::Muted => ((128, 132, 142), 245, Some(Color::DarkGray)),
            Role::Good => ((87, 242, 135), 84, Some(Color::Green)),
            Role::Warn => ((254, 231, 92), 221, Some(Color::Yellow)),
            Role::Bad => ((237, 66, 69), 203, Some(Color::Red)),
            Role::Info => ((86, 204, 242), 81, Some(Color::Cyan)),
            Role::Thinking => ((181, 137, 255), 141, Some(Color::Magenta)),
            Role::OnPill => ((24, 25, 28), 234, Some(Color::Black)),
            Role::Str => ((152, 195, 121), 114, Some(Color::Green)),
            Role::Num => ((209, 154, 102), 179, Some(Color::Yellow)),
        };
        match self.mode {
            ColorMode::None => None,
            ColorMode::Ansi16 => named,
            ColorMode::Indexed => Some(Color::Indexed(idx)),
            ColorMode::TrueColor => Some(Color::Rgb(rgb.0, rgb.1, rgb.2)),
        }
    }

    /// Foreground in `role`'s color. Without colors, the roles that must
    /// stand out get a modifier instead.
    pub fn fg(&self, role: Role) -> Style {
        match self.color(role) {
            Some(c) => Style::new().fg(c),
            None => Style::new().add_modifier(match role {
                Role::Accent | Role::Bad | Role::Warn => Modifier::BOLD,
                Role::Muted => Modifier::DIM,
                _ => Modifier::empty(),
            }),
        }
    }

    /// A pill: dark text on `role`'s color, or reversed video without colors.
    pub fn pill(&self, role: Role) -> Style {
        match (self.color(role), self.color(Role::OnPill)) {
            (Some(bg), Some(fg)) => Style::new().bg(bg).fg(fg).add_modifier(Modifier::BOLD),
            _ => Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD),
        }
    }

    /// The Discord card's surface (no background in 16 colors or none).
    pub fn card(&self) -> Style {
        match self.mode {
            // Light text on the dark card, even in a light terminal.
            ColorMode::TrueColor => Style::new().bg(Color::Rgb(43, 45, 49)).fg(Color::Rgb(242, 243, 245)),
            ColorMode::Indexed => Style::new().bg(Color::Indexed(236)).fg(Color::Indexed(255)),
            ColorMode::Ansi16 | ColorMode::None => Style::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| vars.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
    }

    #[test]
    fn detects_color_modes() {
        let mode = |vars: &[(&str, &str)], windows| Palette::detect_on(env(vars), windows).mode;
        assert_eq!(mode(&[], false), ColorMode::Ansi16);
        assert_eq!(mode(&[("TERM", "xterm")], false), ColorMode::Ansi16);
        assert_eq!(mode(&[("TERM", "xterm-256color")], false), ColorMode::Indexed);
        assert_eq!(mode(&[("TERM", "screen-256color"), ("COLORTERM", "truecolor")], false), ColorMode::TrueColor);
        assert_eq!(mode(&[("COLORTERM", "24bit")], false), ColorMode::TrueColor);
        assert_eq!(mode(&[("COLORTERM", "TrueColor")], false), ColorMode::TrueColor);
        assert_eq!(mode(&[("COLORTERM", "yes"), ("TERM", "xterm")], false), ColorMode::Ansi16);
        // NO_COLOR wins over everything, but only when non-empty (no-color.org).
        assert_eq!(mode(&[("NO_COLOR", "1"), ("COLORTERM", "truecolor")], false), ColorMode::None);
        assert_eq!(mode(&[("NO_COLOR", ""), ("COLORTERM", "truecolor")], false), ColorMode::TrueColor);
        // Windows Terminal: RGB without COLORTERM, on Windows only.
        assert_eq!(mode(&[("WT_SESSION", "abc")], true), ColorMode::TrueColor);
        assert_eq!(mode(&[("WT_SESSION", "abc")], false), ColorMode::Ansi16);
        assert_eq!(mode(&[("WT_SESSION", "abc"), ("NO_COLOR", "x")], true), ColorMode::None);
        assert_eq!(mode(&[], true), ColorMode::Ansi16);
    }

    #[test]
    fn one_theme_maps_onto_every_mode() {
        let roles = [
            Role::Accent,
            Role::Discord,
            Role::Muted,
            Role::Good,
            Role::Warn,
            Role::Bad,
            Role::Info,
            Role::Thinking,
            Role::OnPill,
            Role::Str,
            Role::Num,
        ];
        for mode in [ColorMode::None, ColorMode::Ansi16, ColorMode::Indexed, ColorMode::TrueColor] {
            let p = Palette { mode };
            for r in roles {
                match (mode, p.color(r)) {
                    (ColorMode::None, c) => assert_eq!(c, None),
                    (ColorMode::Ansi16, Some(Color::Rgb(..) | Color::Indexed(_))) => panic!("{r:?}"),
                    (ColorMode::Ansi16, _) => {}
                    (ColorMode::Indexed, c) => assert!(matches!(c, Some(Color::Indexed(_))), "{r:?}"),
                    (ColorMode::TrueColor, c) => assert!(matches!(c, Some(Color::Rgb(..))), "{r:?}"),
                }
            }
        }
    }

    #[test]
    fn no_color_uses_modifiers_only() {
        let p = Palette { mode: ColorMode::None };
        for s in [p.fg(Role::Accent), p.fg(Role::Muted), p.pill(Role::Good), p.card()] {
            assert_eq!((s.fg, s.bg), (None, None), "{s:?}");
        }
        assert!(p.pill(Role::Good).add_modifier.contains(Modifier::REVERSED));
        assert!(p.fg(Role::Accent).add_modifier.contains(Modifier::BOLD));
        assert!(p.fg(Role::Muted).add_modifier.contains(Modifier::DIM));
        // With colors, pills are colored rather than reversed.
        let p = Palette { mode: ColorMode::Ansi16 };
        assert_eq!(p.pill(Role::Good).bg, Some(Color::Green));
        assert!(!p.pill(Role::Good).add_modifier.contains(Modifier::REVERSED));
    }
}
