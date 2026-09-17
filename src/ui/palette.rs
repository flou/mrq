//! The 25 named swatches a skin is made of, and the hex parsing behind them.

use crate::term::caps::Rgb;

/// Declare the palette: the struct, the name table, the accessors and the two lookups
/// that address a swatch by name.
///
/// A macro rather than 25 hand-written fields and 25 accessors because the name table and
/// the struct have to agree — `from_hexes` reads the hex values positionally, and a field
/// added to one list and not the other would silently shift every colour after it.
macro_rules! palette_swatches {
    ($($name:ident),+ $(,)?) => {
        /// One skin's colours, in Catppuccin's slot names.
        ///
        /// Fields, not accessors: a role that does not yet draw from a swatch would
        /// otherwise need a getter nothing calls, and clippy is right to reject that.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub struct Palette {
            $(pub $name: Rgb),+
        }

        /// Every swatch name, in the order [`Palette::from_hexes`] reads them.
        pub const SWATCHES: &[&str] = &[$(stringify!($name)),+];

        impl Palette {
            /// Build a palette from hex strings, in [`SWATCHES`] order.
            ///
            /// `None` when any value is not a hex colour, so a malformed built-in is a
            /// test failure rather than a black screen.
            pub fn from_hexes(hexes: [&str; COUNT]) -> Option<Self> {
                let mut next = hexes.iter();
                $(let $name = parse_hex(next.next()?)?;)+
                Some(Self { $($name),+ })
            }

            /// Replace one swatch by name, reporting whether the name is a swatch at all.
            pub fn set(&mut self, swatch: &str, colour: Rgb) -> bool {
                match swatch {
                    $(stringify!($name) => {
                        self.$name = colour;
                        true
                    })+
                    _ => false,
                }
            }
        }
    };
}

palette_swatches!(
    rosewater, flamingo, pink, mauve, red, maroon, peach, yellow, green, teal, sky, sapphire, blue,
    lavender, text, subtext1, subtext0, overlay1, overlay0, surface2, surface1, surface0, base,
    mantle, crust,
);

/// How many swatches a skin has to provide.
pub const COUNT: usize = SWATCHES.len();

/// Parse `#rrggbb` or `rrggbb`, case-insensitive.
pub fn parse_hex(hex: &str) -> Option<Rgb> {
    let body = hex.strip_prefix('#').unwrap_or(hex);
    if body.len() != 6 || !body.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }

    let byte = |at: usize| u8::from_str_radix(&body[at..at + 2], 16).ok();
    Some(Rgb {
        r: byte(0)?,
        g: byte(2)?,
        b: byte(4)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette() -> Palette {
        Palette::from_hexes([
            "#f5e0dc", "#f2cdcd", "#f5c2e7", "#cba6f7", "#f38ba8", "#eba0ac", "#fab387", "#f9e2af",
            "#a6e3a1", "#94e2d5", "#89dceb", "#74c7ec", "#89b4fa", "#b4befe", "#cdd6f4", "#bac2de",
            "#a6adc8", "#7f849c", "#6c7086", "#585b70", "#45475a", "#313244", "#1e1e2e", "#181825",
            "#11111b",
        ])
        .unwrap()
    }

    #[test]
    fn there_are_twenty_five_uniquely_named_swatches() {
        assert_eq!(COUNT, 25);

        let mut names = SWATCHES.to_vec();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), COUNT, "swatch names must be unique");
    }

    /// The hexes are read positionally, so the first and last swatch landing in the right
    /// field is what says the name table and the struct still agree.
    #[test]
    fn hexes_land_in_declaration_order() {
        let p = palette();
        assert_eq!(p.rosewater, parse_hex("#f5e0dc").unwrap());
        assert_eq!(p.text, parse_hex("#cdd6f4").unwrap());
        assert_eq!(p.crust, parse_hex("#11111b").unwrap());
    }

    #[test]
    fn every_swatch_name_addresses_a_field() {
        for name in SWATCHES {
            let mut p = palette();
            assert!(p.set(name, Rgb { r: 1, g: 2, b: 3 }), "{name}");
        }
        assert!(!palette().set("chartreuse", Rgb { r: 0, g: 0, b: 0 }));
    }

    #[test]
    fn setting_one_swatch_leaves_the_others_alone() {
        let mut p = palette();
        p.set("red", Rgb { r: 1, g: 2, b: 3 })
            .then_some(())
            .unwrap();

        assert_eq!(p.red, Rgb { r: 1, g: 2, b: 3 });
        assert_eq!(p.green, palette().green);
    }

    #[test]
    fn hex_parsing_accepts_both_forms_and_rejects_the_rest() {
        assert_eq!(
            parse_hex("#FF8000"),
            Some(Rgb {
                r: 255,
                g: 128,
                b: 0
            })
        );
        assert_eq!(
            parse_hex("ff8000"),
            Some(Rgb {
                r: 255,
                g: 128,
                b: 0
            })
        );

        for bad in ["", "#fff", "#gggggg", "#ff80000", "12345", "#ff 000"] {
            assert_eq!(parse_hex(bad), None, "`{bad}` should be rejected");
        }
    }

    #[test]
    fn a_malformed_hex_fails_the_whole_palette() {
        let mut hexes = ["#000000"; COUNT];
        hexes[7] = "not-a-colour";
        assert_eq!(Palette::from_hexes(hexes), None);
    }
}
