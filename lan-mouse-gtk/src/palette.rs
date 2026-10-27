//! Window colors for each palette and light/dark mode.
//!
//! `style.css` only references the `sm_*` named colors; this module
//! generates their definitions, plus the libadwaita surface colors, for
//! the active palette. Regenerating one small provider when the palette
//! or the dark state changes keeps the static stylesheet free of
//! per-palette selectors and works on GTK runtimes without CSS variables.

use std::fmt::Write;

use crate::preferences::Palette;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Colors {
    pub bg: &'static str,
    pub card: &'static str,
    pub sunk: &'static str,
    pub fg: &'static str,
    pub dim: &'static str,
    pub faint: &'static str,
    pub accent: &'static str,
    pub warn: &'static str,
}

pub fn colors(palette: Palette, dark: bool) -> Colors {
    match (palette, dark) {
        (Palette::Ember, false) => Colors {
            bg: "#F4F2EF",
            card: "#FBFAF8",
            sunk: "#EAE6E1",
            fg: "#1F1C19",
            dim: "#6F6862",
            faint: "#A9A29B",
            accent: "#C8682F",
            warn: "#B8432F",
        },
        (Palette::Ember, true) => Colors {
            bg: "#151311",
            card: "#1D1A17",
            sunk: "#25211D",
            fg: "#EEE9E3",
            dim: "#9C948C",
            faint: "#625B54",
            accent: "#EE8D50",
            warn: "#E06B55",
        },
        (Palette::Pine, false) => Colors {
            bg: "#F1F3F0",
            card: "#F9FAF8",
            sunk: "#E3E8E3",
            fg: "#18201C",
            dim: "#637069",
            faint: "#9DA8A1",
            accent: "#2E7A57",
            warn: "#B4483A",
        },
        (Palette::Pine, true) => Colors {
            bg: "#101412",
            card: "#161C19",
            sunk: "#1D2420",
            fg: "#E6ECE8",
            dim: "#8F9C95",
            faint: "#57635C",
            accent: "#5BC496",
            warn: "#E07062",
        },
    }
}

/// Named-color definitions for `palette`. With `variables`, also emit
/// the CSS custom properties that libadwaita 1.6+ reads instead of the
/// deprecated named colors; older GTK cannot parse those.
pub fn css(palette: Palette, dark: bool, variables: bool) -> String {
    let c = colors(palette, dark);
    // A bright dark-mode accent needs dark text on top of it.
    let on_accent = if dark { c.bg } else { "#FFFFFF" };
    let named = [
        ("sm_bg", c.bg),
        ("sm_card", c.card),
        ("sm_sunk", c.sunk),
        ("sm_fg", c.fg),
        ("sm_dim", c.dim),
        ("sm_faint", c.faint),
        ("sm_accent", c.accent),
        ("sm_warn", c.warn),
        ("sm_on_accent", on_accent),
        ("window_bg_color", c.bg),
        ("window_fg_color", c.fg),
        ("view_bg_color", c.card),
        ("view_fg_color", c.fg),
        ("headerbar_bg_color", c.bg),
        ("headerbar_fg_color", c.fg),
        ("headerbar_backdrop_color", c.bg),
        ("card_bg_color", c.card),
        ("card_fg_color", c.fg),
        ("dialog_bg_color", c.card),
        ("dialog_fg_color", c.fg),
        ("popover_bg_color", c.card),
        ("popover_fg_color", c.fg),
        ("accent_color", c.accent),
        ("accent_bg_color", c.accent),
        ("accent_fg_color", on_accent),
        ("destructive_color", c.warn),
        ("destructive_bg_color", c.warn),
        ("error_color", c.warn),
        ("error_bg_color", c.warn),
    ];
    let mut out = String::new();
    for (name, value) in named {
        let _ = writeln!(out, "@define-color {name} {value};");
    }
    if variables {
        out.push_str(":root {\n");
        for (name, value) in named.iter().filter(|(name, _)| !name.starts_with("sm_")) {
            let _ = writeln!(out, "  --{}: {value};", name.replace('_', "-"));
        }
        out.push_str("  --headerbar-shade-color: transparent;\n}\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palettes_match_the_design_table() {
        assert_eq!(colors(Palette::Ember, false).accent, "#C8682F");
        assert_eq!(colors(Palette::Ember, true).bg, "#151311");
        assert_eq!(colors(Palette::Pine, false).card, "#F9FAF8");
        assert_eq!(colors(Palette::Pine, true).accent, "#5BC496");
    }

    #[test]
    fn css_defines_every_color_the_stylesheet_uses() {
        let css = css(Palette::Pine, true, false);
        // Drop comments so prose such as "@sm_* colors" is not a reference.
        let stylesheet: String = include_str!("../resources/style.css")
            .split("/*")
            .enumerate()
            .map(|(i, part)| {
                if i == 0 {
                    part
                } else {
                    part.split_once("*/").map_or("", |(_, rest)| rest)
                }
            })
            .collect();
        for name in stylesheet
            .split('@')
            .skip(1)
            .filter_map(|rest| {
                rest.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                    .next()
            })
            .filter(|name| name.starts_with("sm_"))
        {
            assert!(
                css.contains(&format!("@define-color {name} ")),
                "{name} is used by style.css but not defined"
            );
        }
        assert!(!css.contains(":root"));
    }

    #[test]
    fn variables_are_only_emitted_on_request() {
        let css = css(Palette::Ember, false, true);
        assert!(css.contains("--window-bg-color: #F4F2EF;"));
        assert!(css.contains("--accent-fg-color: #FFFFFF;"));
        assert!(!css.contains("--sm-"));
    }
}
