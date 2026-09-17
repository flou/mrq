//! The built-in skins: one table of 25 hex values per name, plus alias and auto lookup.

use crate::ui::palette::{COUNT, Palette};

/// Every skin `[skin].name` accepts, in the order the picker lists them.
pub const BUILTIN_NAMES: [&str; 12] = [
    "catppuccin-mocha",
    "catppuccin-macchiato",
    "catppuccin-frappe",
    "dracula",
    "flexoki-dark",
    "gruvbox-dark",
    "monokai",
    "nord",
    "one-dark",
    "rose-pine",
    "solarized-dark",
    "tokyo-night",
];

/// What `[skin].name` is set to for "use the dark default".
pub const AUTO: &str = "auto";

/// The skin `auto` resolves to. There is only ever a dark one — dark-on-dark is
/// unreadable, while a light palette on a dark terminal is merely low-contrast, and mrq
/// ships no light skins to fall back to instead.
pub const AUTO_DARK: &str = "catppuccin-mocha";

/// Shorthands, so `mocha` and `gruvbox` mean what a user expects them to.
const ALIASES: &[(&str, &str)] = &[
    ("catppuccin", "catppuccin-mocha"),
    ("mocha", "catppuccin-mocha"),
    ("macchiato", "catppuccin-macchiato"),
    ("frappe", "catppuccin-frappe"),
    ("frappé", "catppuccin-frappe"),
    ("flexoki", "flexoki-dark"),
    ("gruvbox", "gruvbox-dark"),
    ("onedark", "one-dark"),
    ("rosepine", "rose-pine"),
    ("solarized", "solarized-dark"),
    ("tokyonight", "tokyo-night"),
];

/// The canonical name a skin is known by, resolving aliases and letter case.
pub fn canonical(name: &str) -> Option<&'static str> {
    let wanted = name.trim().to_lowercase();
    BUILTIN_NAMES
        .into_iter()
        .find(|builtin| *builtin == wanted)
        .or_else(|| {
            ALIASES
                .iter()
                .find(|(alias, _)| *alias == wanted)
                .map(|(_, target)| *target)
        })
}

/// The palette for a skin name, alias or otherwise.
pub fn palette(name: &str) -> Option<Palette> {
    let canonical = canonical(name)?;
    // A built-in whose table is malformed would leave the UI colourless with no
    // explanation; `every_builtin_parses` is what stops that reaching a release.
    Palette::from_hexes(hexes(canonical)?)
}

/// The hex table for a canonical name, in [`crate::ui::palette::SWATCHES`] order:
/// rosewater, flamingo, pink, mauve, red, maroon, peach, yellow, green, teal, sky,
/// sapphire, blue, lavender, text, subtext1, subtext0, overlay1, overlay0, surface2,
/// surface1, surface0, base, mantle, crust.
///
/// Palettes narrower than 25 colours — most of them — repeat a colour across neighbouring
/// slots rather than inventing values that are not in the scheme.
fn hexes(canonical: &str) -> Option<[&'static str; COUNT]> {
    Some(match canonical {
        "catppuccin-mocha" => [
            "#f5e0dc", "#f2cdcd", "#f5c2e7", "#cba6f7", "#f38ba8", "#eba0ac", "#fab387", "#f9e2af",
            "#a6e3a1", "#94e2d5", "#89dceb", "#74c7ec", "#89b4fa", "#b4befe", "#cdd6f4", "#bac2de",
            "#a6adc8", "#7f849c", "#6c7086", "#585b70", "#45475a", "#313244", "#1e1e2e", "#181825",
            "#11111b",
        ],
        "catppuccin-macchiato" => [
            "#f4dbd6", "#f0c6c6", "#f5bde6", "#c6a0f6", "#ed8796", "#ee99a0", "#f5a97f", "#eed49f",
            "#a6da95", "#8bd5ca", "#91d7e3", "#7dc4e4", "#8aadf4", "#b7bdf8", "#cad3f5", "#b8c0e0",
            "#a5adcb", "#8087a2", "#6e738d", "#5b6078", "#494d64", "#363a4f", "#24273a", "#1e2030",
            "#181926",
        ],
        "catppuccin-frappe" => [
            "#f2d5cf", "#eebebe", "#f4b8e4", "#ca9ee6", "#e78284", "#ea999c", "#ef9f76", "#e5c890",
            "#a6d189", "#81c8be", "#99d1db", "#85c1dc", "#8caaee", "#babbf1", "#c6d0f5", "#b5bfe2",
            "#a5adce", "#838ba7", "#737994", "#626880", "#51576d", "#414559", "#303446", "#292c3c",
            "#232634",
        ],
        "dracula" => [
            "#f8f8f2", "#ff9580", "#ff79c6", "#bd93f9", "#ff5555", "#ff6e6e", "#ffb86c", "#f1fa8c",
            "#50fa7b", "#8be9fd", "#8be9fd", "#8be9fd", "#8be9fd", "#bd93f9", "#f8f8f2", "#e2e2dc",
            "#c8c8c2", "#8a8fa8", "#6272a4", "#565872", "#44475a", "#343746", "#282a36", "#21222c",
            "#191a21",
        ],
        "flexoki-dark" => [
            "#cecdc3", "#d14d41", "#ce5d97", "#8b7ec8", "#d14d41", "#af3029", "#da702c", "#d0a215",
            "#879a39", "#3aa99f", "#3aa99f", "#4385be", "#4385be", "#8b7ec8", "#cecdc3", "#b7b5ac",
            "#878580", "#6f6e69", "#575653", "#403e3c", "#343331", "#282726", "#100f0f", "#1c1b1a",
            "#0a0a0a",
        ],
        "gruvbox-dark" => [
            "#ebdbb2", "#f2e5bc", "#d3869b", "#d3869b", "#fb4934", "#f2594b", "#fe8019", "#fabd2f",
            "#b8bb26", "#8ec07c", "#8ec07c", "#83a598", "#83a598", "#d3869b", "#ebdbb2", "#d5c4a1",
            "#bdae93", "#a89984", "#928374", "#665c54", "#504945", "#3c3836", "#282828", "#1d2021",
            "#161616",
        ],
        "monokai" => [
            "#f8f8f2", "#fd971f", "#f92672", "#ae81ff", "#f92672", "#f92672", "#fd971f", "#e6db74",
            "#a6e22e", "#66d9ef", "#66d9ef", "#66d9ef", "#66d9ef", "#ae81ff", "#f8f8f2", "#e0e0d8",
            "#c8c8c0", "#90897a", "#75715e", "#5a5a50", "#49483e", "#3e3d32", "#272822", "#1f201a",
            "#171812",
        ],
        "nord" => [
            "#eceff4", "#e5e9f0", "#b48ead", "#b48ead", "#bf616a", "#bf616a", "#d08770", "#ebcb8b",
            "#a3be8c", "#8fbcbb", "#88c0d0", "#81a1c1", "#81a1c1", "#b48ead", "#d8dee9", "#c8ced9",
            "#aeb6c4", "#8b93a3", "#6b7484", "#4c566a", "#434c5e", "#3b4252", "#2e3440", "#2b303b",
            "#242933",
        ],
        "one-dark" => [
            "#dcdfe4", "#e06c75", "#c678dd", "#c678dd", "#e06c75", "#be5046", "#d19a66", "#e5c07b",
            "#98c379", "#56b6c2", "#56b6c2", "#61afef", "#61afef", "#c678dd", "#abb2bf", "#9da5b4",
            "#8b92a5", "#6b7280", "#5c6370", "#4b5263", "#3e4451", "#333842", "#282c34", "#21252b",
            "#1b1f23",
        ],
        "rose-pine" => [
            "#ebbcba", "#ebbcba", "#eb6f92", "#c4a7e7", "#eb6f92", "#eb6f92", "#f6c177", "#f6c177",
            "#9ccfd8", "#9ccfd8", "#9ccfd8", "#31748f", "#31748f", "#c4a7e7", "#e0def4", "#908caa",
            "#908caa", "#6e6a86", "#6e6a86", "#524f67", "#403d52", "#26233a", "#191724", "#1f1d2e",
            "#21202e",
        ],
        "solarized-dark" => [
            "#eee8d5", "#cb4b16", "#d33682", "#6c71c4", "#dc322f", "#cb4b16", "#cb4b16", "#b58900",
            "#859900", "#2aa198", "#2aa198", "#268bd2", "#268bd2", "#6c71c4", "#93a1a1", "#839496",
            "#708284", "#657b83", "#586e75", "#17505d", "#0e4652", "#073642", "#002b36", "#00252e",
            "#001e26",
        ],
        "tokyo-night" => [
            "#cfc9c2", "#ff9e64", "#bb9af7", "#9d7cd8", "#f7768e", "#ff757f", "#ff9e64", "#e0af68",
            "#9ece6a", "#73daca", "#7dcfff", "#2ac3de", "#7aa2f7", "#b4f9f8", "#c0caf5", "#a9b1d6",
            "#9aa5ce", "#737aa2", "#565f89", "#414868", "#292e42", "#24283b", "#1a1b26", "#16161e",
            "#101014",
        ],
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::palette::SWATCHES;

    /// A built-in whose table is malformed or the wrong length leaves the UI colourless
    /// with no explanation at all, so it has to fail here instead.
    #[test]
    fn every_builtin_parses() {
        for name in BUILTIN_NAMES {
            assert!(palette(name).is_some(), "{name} does not resolve");
        }
    }

    #[test]
    fn names_are_unique_and_the_default_leads() {
        let mut names = BUILTIN_NAMES.to_vec();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), BUILTIN_NAMES.len(), "duplicate skin name");

        // The picker opens on the skin in force, so the shipped default first is what
        // makes the list read top-down for a user who has not configured one.
        assert_eq!(BUILTIN_NAMES[0], AUTO_DARK);
    }

    #[test]
    fn lookup_is_case_insensitive_and_resolves_aliases() {
        assert_eq!(canonical("Catppuccin-Mocha"), Some("catppuccin-mocha"));
        assert_eq!(canonical("  mocha "), Some("catppuccin-mocha"));
        assert_eq!(canonical("gruvbox"), Some("gruvbox-dark"));
        assert_eq!(canonical("solarized"), Some("solarized-dark"));
        assert_eq!(canonical("chartreuse"), None);
        assert_eq!(canonical(AUTO), None, "auto is resolved before lookup");
    }

    #[test]
    fn every_alias_points_at_a_builtin() {
        for (alias, target) in ALIASES {
            assert!(BUILTIN_NAMES.contains(target), "{alias} -> {target}");
            assert!(
                !BUILTIN_NAMES.contains(alias),
                "{alias} is already a built-in name"
            );
        }
    }

    /// The surfaces have to sit on the same side of the line as the background they cover,
    /// and the text on the other one — a skin that gets this wrong renders as text the
    /// same shade as the panel it is written on.
    #[test]
    fn every_skin_puts_its_text_and_its_surfaces_on_the_right_side_of_its_base() {
        for name in BUILTIN_NAMES {
            let p = palette(name).unwrap();
            let dark = p.base.is_dark();

            assert_eq!(p.surface0.is_dark(), dark, "{name}: surface0");
            assert_eq!(p.surface1.is_dark(), dark, "{name}: surface1");
            assert_eq!(p.mantle.is_dark(), dark, "{name}: mantle");
            assert_ne!(p.text.is_dark(), dark, "{name}: text");
        }
    }

    /// Positional tables are easy to shift by one, and the tell is a skin whose `base` is
    /// not the darkest thing in it (or the lightest, on a light skin).
    #[test]
    fn base_is_the_extreme_of_every_skin() {
        for name in BUILTIN_NAMES {
            let p = palette(name).unwrap();
            let base = p.base.luma();
            let surfaces = [p.surface0, p.surface1, p.surface2, p.text];

            for swatch in surfaces {
                if p.base.is_dark() {
                    assert!(swatch.luma() > base, "{name}: base is not the darkest");
                } else {
                    assert!(swatch.luma() < base, "{name}: base is not the lightest");
                }
            }
        }
    }

    #[test]
    fn a_skin_provides_one_value_per_swatch() {
        assert_eq!(hexes("nord").unwrap().len(), SWATCHES.len());
        assert!(hexes("chartreuse").is_none());
    }
}
