//! Colours and glyphs, in the one place they are allowed to exist.
//!
//! Call sites ask for a [`Role`] — "this is a failure", never for a colour — which is what
//! lets one skin render on a truecolor terminal, a 256-colour one and a serial console.

use std::borrow::Cow;

use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::border;
use ratatui::widgets::{Block, Borders};

use crate::app::state::Attention;
use crate::config::schema::Skin;
use crate::error::ConfigError;
use crate::gitlab::model::PipelineStatus;
use crate::term::caps::{Capabilities, ColorDepth, Rgb};
use crate::ui::palette::{self, Palette};
use crate::ui::skins;

/// What a piece of text means, independent of how it is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Ordinary table text.
    Normal,
    /// De-emphasised: drafts, skipped pipelines, absent values.
    Dim,
    /// Tab names, panel titles and popup headings.
    Header,
    /// The table's column header row.
    ColumnHeader,
    /// The frame around the table, and any other panel border.
    Border,
    /// The selected row.
    Selection,
    /// Something succeeded.
    Success,
    /// Something failed and needs attention.
    Failure,
    /// In progress, or needs a decision.
    Pending,
    /// A problem that is not a failure: conflicts, degraded queries.
    Warning,
    /// Added lines.
    Added,
    /// Removed lines.
    Removed,
    /// A link or an actionable hint.
    Accent,
    /// The gutter marker on the selected row.
    Marker,
}

/// A resolved theme: the skin's palette, the depth it is rendered at, and whether glyphs
/// are Unicode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    depth: ColorDepth,
    ascii: bool,
    skin: String,
    palette: Palette,
    /// The `[skin.colors]` overrides, kept rather than only applied, so switching skin
    /// from inside the TUI re-applies them instead of silently dropping them.
    overrides: Vec<(String, Rgb)>,
}

impl Theme {
    /// Resolve the configured skin against what the terminal can do.
    ///
    /// Anything [`check`] would have rejected is ignored here rather than failing: the
    /// report belongs on a terminal that is still showing the user's shell, which is long
    /// before this runs.
    pub fn resolve(skin: &Skin, ascii: bool, caps: &Capabilities) -> Self {
        let name = if skin.name.trim().eq_ignore_ascii_case(skins::AUTO) {
            skins::AUTO_DARK
        } else {
            &skin.name
        };
        let overrides = skin
            .colors
            .iter()
            .filter_map(|(swatch, hex)| Some((swatch.clone(), palette::parse_hex(hex)?)))
            .collect();

        Self::build(name, ascii, caps.color, overrides)
    }

    /// A built-in skin with no overrides.
    #[cfg(test)]
    pub(crate) fn builtin(name: &str, ascii: bool, caps: &Capabilities) -> Self {
        Self::build(name, ascii, caps.color, Vec::new())
    }

    /// The same theme wearing a different skin, for the skin picker.
    #[must_use]
    pub fn with_skin(&self, name: &str) -> Self {
        Self::build(name, self.ascii, self.depth, self.overrides.clone())
    }

    fn build(name: &str, ascii: bool, depth: ColorDepth, overrides: Vec<(String, Rgb)>) -> Self {
        let skin = skins::canonical(name).unwrap_or(skins::AUTO_DARK);
        #[expect(
            clippy::expect_used,
            reason = "every canonical name resolving to a palette is a pinned test invariant"
        )]
        let mut palette =
            skins::palette(skin).expect("a canonical skin name resolves to a palette");
        for (swatch, colour) in &overrides {
            palette.set(swatch, *colour);
        }

        Self {
            depth,
            ascii,
            skin: skin.to_owned(),
            palette,
            overrides,
        }
    }

    /// The skin in force, by canonical name.
    pub fn skin(&self) -> &str {
        &self.skin
    }

    /// The colour every unpainted cell shows, at the theme's colour depth.
    ///
    /// Nothing at 16 colours or fewer: the terminal's own palette adapts, and overriding
    /// its background is how text ends up invisible — the same reason ansi16 carries no
    /// background variants.
    pub fn base_colour(&self) -> Color {
        match self.depth {
            ColorDepth::None | ColorDepth::Ansi16 => Color::Reset,
            ColorDepth::Indexed256 => indexed(self.palette.base),
            ColorDepth::TrueColor => truecolor(self.palette.base),
        }
    }

    #[cfg(test)]
    pub(crate) const fn depth(&self) -> ColorDepth {
        self.depth
    }

    #[cfg(test)]
    pub(crate) const fn is_ascii(&self) -> bool {
        self.ascii
    }

    /// The style for a role.
    pub fn style(&self, role: Role) -> Style {
        let style = Style::default();
        match role {
            // A soft background where the terminal has the colours for one, so the cursor
            // row reads as a highlight rather than a block of inverted video. Below that
            // depth reverse video is the only emphasis left, so it is paired with a
            // gutter marker for terminals where reverse is weak.
            Role::Selection => match self.selection_background() {
                Some(colour) => style.bg(colour),
                None => style.add_modifier(Modifier::REVERSED),
            },
            Role::Header => match self.colour(role) {
                Some(colour) => style.fg(colour).add_modifier(Modifier::BOLD),
                None => style.add_modifier(Modifier::BOLD),
            },
            // Not bold: unlike `Role::Header`, the colour alone is enough to set the
            // column header row apart from the body text below it.
            Role::ColumnHeader => match self.colour(role) {
                Some(colour) => style.fg(colour),
                None => style.add_modifier(Modifier::BOLD),
            },
            Role::Dim => match self.colour(role) {
                Some(colour) => style.fg(colour),
                // With no colour, DIM is the only way to de-emphasise.
                None => style.add_modifier(Modifier::DIM),
            },
            _ => match self.colour(role) {
                Some(colour) => style.fg(colour),
                None => style,
            },
        }
    }

    /// The style for a role, layered with bold so it stands out from its own colour.
    ///
    /// Used where a single role needs both a colour and an extra emphasis that would
    /// otherwise dilute the role if it were baked into `style`.
    pub fn emphasise(&self, role: Role) -> Style {
        self.style(role).add_modifier(Modifier::BOLD)
    }

    /// The colour for a role, or `None` when the terminal has no colour.
    fn colour(&self, role: Role) -> Option<Color> {
        match self.depth {
            ColorDepth::None => None,
            ColorDepth::Ansi16 => Some(self.ansi16(role)),
            ColorDepth::Indexed256 => Some(indexed(self.swatch(role))),
            ColorDepth::TrueColor => Some(truecolor(self.swatch(role))),
        }
    }

    /// Which swatch of the skin a role draws from.
    ///
    /// The whole mapping from meaning to colour is here, so a skin is 25 hex values and
    /// nothing else — and so a new skin cannot forget to define one of the states.
    const fn swatch(&self, role: Role) -> Rgb {
        let skin = &self.palette;
        match role {
            // The selection is drawn as a background; the foreground stays body text so
            // the highlighted row reads the same as the rest of the table.
            Role::Normal | Role::Selection => skin.text,
            Role::Dim => skin.overlay1,
            Role::Header => skin.blue,
            Role::ColumnHeader => skin.yellow,
            // Deliberately low-contrast: the frame is orientation, not information.
            Role::Border => skin.surface1,
            Role::Success | Role::Added => skin.green,
            Role::Failure | Role::Removed => skin.red,
            Role::Pending => skin.yellow,
            // Not yellow, or a conflicted merge request and a pending pipeline would be
            // the same colour — and they mean opposite things about whether to look.
            Role::Warning => skin.peach,
            Role::Accent => skin.sapphire,
            Role::Marker => skin.yellow,
        }
    }

    /// The 16 ANSI names. No background variants: the terminal's own palette already
    /// adapts, and overriding it is how text ends up invisible.
    const fn ansi16(&self, role: Role) -> Color {
        match role {
            Role::Normal | Role::Selection => Color::Reset,
            Role::Dim | Role::Border => Color::DarkGray,
            Role::Header => Color::Cyan,
            Role::Success | Role::Added => Color::Green,
            Role::Failure | Role::Removed => Color::Red,
            Role::Pending | Role::Warning | Role::Marker | Role::ColumnHeader => Color::Yellow,
            Role::Accent => Color::Blue,
        }
    }

    /// The cursor row's background, or `None` when the terminal cannot draw one softly
    /// enough to be an improvement on reverse video.
    fn selection_background(&self) -> Option<Color> {
        match self.depth {
            ColorDepth::None | ColorDepth::Ansi16 => None,
            ColorDepth::Indexed256 => Some(indexed(self.palette.surface0)),
            ColorDepth::TrueColor => Some(truecolor(self.palette.surface0)),
        }
    }

    /// The frame around a panel: rounded where the font can draw it, `+-|` where the
    /// ascii theme says it cannot.
    pub const fn border_set(&self) -> border::Set<'static> {
        if self.ascii {
            border::Set {
                top_left: "+",
                top_right: "+",
                bottom_left: "+",
                bottom_right: "+",
                vertical_left: "|",
                vertical_right: "|",
                horizontal_top: "-",
                horizontal_bottom: "-",
            }
        } else {
            border::ROUNDED
        }
    }

    /// A bordered panel carrying `title`, the one way this program draws a frame.
    ///
    /// The background is set explicitly rather than left to whatever the frame already
    /// painted: a popup is preceded by `Clear`, which hard-resets its area to the
    /// terminal's own default colour, so without this a skin preview would leave the
    /// popup's background stuck on that default instead of following the theme.
    pub fn panel<'a>(&self, title: impl Into<ratatui::text::Line<'a>>) -> Block<'a> {
        Block::default()
            .style(Style::default().bg(self.base_colour()))
            .borders(Borders::ALL)
            .border_set(self.border_set())
            .border_style(self.style(Role::Border))
            .title(title)
            .title_style(self.style(Role::Header))
    }

    /// The glyph and role for a pipeline status.
    pub const fn pipeline(&self, status: Option<&PipelineStatus>) -> (&'static str, Role) {
        let Some(status) = status else {
            return (" ", Role::Dim);
        };

        match status {
            PipelineStatus::Success => (if self.ascii { "+" } else { "✔" }, Role::Success),
            PipelineStatus::Failed => (if self.ascii { "x" } else { "✘" }, Role::Failure),
            PipelineStatus::Running => (if self.ascii { "~" } else { "●" }, Role::Pending),
            PipelineStatus::Created
            | PipelineStatus::WaitingForResource
            | PipelineStatus::Preparing
            | PipelineStatus::Pending => (if self.ascii { "." } else { "◌" }, Role::Pending),
            PipelineStatus::Canceled => (if self.ascii { "-" } else { "⊘" }, Role::Dim),
            PipelineStatus::Skipped => (if self.ascii { ">" } else { "»" }, Role::Dim),
            PipelineStatus::Manual | PipelineStatus::Scheduled => {
                (if self.ascii { "=" } else { "⏸" }, Role::Accent)
            }
            // A status this build has never heard of still gets a cell, so the column
            // does not silently misreport it as "no pipeline".
            PipelineStatus::Unknown(_) => (if self.ascii { "?" } else { "�" }, Role::Dim),
        }
    }

    /// The marker in the selection gutter.
    pub const fn selection_marker(&self) -> &'static str {
        if self.ascii { "|" } else { "▎" }
    }

    /// The marker for a merge request that arrived since the last refresh.
    pub const fn new_marker(&self) -> &'static str {
        "*"
    }

    /// The gutter cell when a row is neither selected nor new.
    pub const fn blank_marker(&self) -> &'static str {
        " "
    }

    /// Sort direction indicator for the status bar.
    pub const fn sort_arrow(&self, ascending: bool) -> &'static str {
        match (self.ascii, ascending) {
            (true, true) => "^",
            (true, false) => "v",
            (false, true) => "↑",
            (false, false) => "↓",
        }
    }

    /// The separator between status-bar segments.
    pub const fn separator(&self) -> &'static str {
        if self.ascii { " | " } else { " · " }
    }

    /// The divider between two tabs.
    pub const fn tab_divider(&self) -> &'static str {
        if self.ascii { "|" } else { "│" }
    }

    /// Frames of the activity spinner.
    pub const fn spinner_frames(&self) -> &'static [&'static str] {
        if self.ascii {
            &["|", "/", "-", "\\"]
        } else {
            &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]
        }
    }

    /// The ellipsis used when text is truncated.
    pub const fn ellipsis(&self) -> &'static str {
        if self.ascii { "..." } else { "…" }
    }

    /// The mark for "this merge request is approved".
    pub const fn approved_mark(&self) -> &'static str {
        if self.ascii { "y" } else { "✔" }
    }

    /// The dash introducing a status-bar explanation — `stale — retrying in 8s`.
    pub const fn dash(&self) -> &'static str {
        if self.ascii { " - " } else { " — " }
    }

    /// Makes externally-sourced text (an `Error` message, a GitLab error string) safe to
    /// embed verbatim under the ascii theme. Call sites in this crate already go through
    /// [`Self::dash`] and friends; this exists for the text this module does not control.
    pub fn ascii_safe<'a>(&self, text: &'a str) -> Cow<'a, str> {
        if !self.ascii || text.is_ascii() {
            return Cow::Borrowed(text);
        }
        let replaced = text.replace(['—', '–'], "-").replace('…', "...");
        if replaced.is_ascii() {
            return Cow::Owned(replaced);
        }
        Cow::Owned(
            replaced
                .chars()
                .map(|c| if c.is_ascii() { c } else { '?' })
                .collect(),
        )
    }

    /// The tab marker for a filter that needs attention.
    ///
    /// Two glyphs rather than two colours: the 16-colour and no-colour paths would
    /// otherwise render a degraded tab and a broken one identically.
    pub const fn attention_marker(&self, attention: Attention) -> &'static str {
        match attention {
            Attention::Healthy => "",
            Attention::Degraded => "~",
            Attention::Problem => "!",
        }
    }

    /// How a tab marker is coloured.
    pub const fn attention_role(&self, attention: Attention) -> Role {
        match attention {
            Attention::Healthy => Role::Normal,
            Attention::Degraded => Role::Warning,
            Attention::Problem => Role::Failure,
        }
    }
}

/// Whether a `[skin]` table names things that exist.
///
/// Separate from resolution because the two happen at different moments: this runs while
/// the terminal is still showing the user's shell, and resolution runs after the alternate
/// screen has swallowed it.
pub fn check(skin: &Skin) -> Result<(), ConfigError> {
    let invalid = |key: &str, message: String| ConfigError::Invalid {
        key: key.to_owned(),
        message,
    };

    let name = skin.name.trim();
    if !name.eq_ignore_ascii_case(skins::AUTO) && skins::canonical(name).is_none() {
        return Err(invalid(
            "skin.name",
            format!(
                "unknown skin `{name}`; expected `{}` or one of: {}",
                skins::AUTO,
                skins::BUILTIN_NAMES.join(", ")
            ),
        ));
    }

    for (swatch, hex) in &skin.colors {
        if !palette::SWATCHES.contains(&swatch.as_str()) {
            return Err(invalid(
                &format!("skin.colors.{swatch}"),
                format!(
                    "unknown swatch; expected one of: {}",
                    palette::SWATCHES.join(", ")
                ),
            ));
        }
        if palette::parse_hex(hex).is_none() {
            return Err(invalid(
                &format!("skin.colors.{swatch}"),
                format!("`{hex}` is not a colour; expected `#rrggbb`"),
            ));
        }
    }

    Ok(())
}

const fn truecolor(colour: Rgb) -> Color {
    Color::Rgb(colour.r, colour.g, colour.b)
}

fn indexed(colour: Rgb) -> Color {
    Color::Indexed(nearest_256(colour))
}

/// The xterm-256 entry closest to an RGB value.
///
/// Converting rather than carrying a second hand-tuned table per skin: seventeen skins
/// times twenty-five swatches is not a table anyone would keep honest, and the grey ramp
/// is what stops the near-neutral surface colours snapping to a tinted cube entry.
fn nearest_256(colour: Rgb) -> u8 {
    /// The six levels each axis of the 6x6x6 cube is quantised to.
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

    #[expect(clippy::expect_used, reason = "LEVELS is a non-empty const array")]
    let axis = |value: u8| {
        LEVELS
            .into_iter()
            .enumerate()
            .min_by_key(|(_, level)| i32::from(*level).abs_diff(i32::from(value)))
            .map(|(index, level)| (index as u8, level))
            .expect("the cube has six levels per axis")
    };

    let distance = |a: Rgb, b: Rgb| {
        let square = |x: u8, y: u8| i32::from(x).abs_diff(i32::from(y)).pow(2);
        square(a.r, b.r) + square(a.g, b.g) + square(a.b, b.b)
    };

    let (ri, r) = axis(colour.r);
    let (gi, g) = axis(colour.g);
    let (bi, b) = axis(colour.b);
    let cube = Rgb { r, g, b };

    let step = ((colour.luma() - 8.0) / 10.0).round().clamp(0.0, 23.0) as u8;
    let level = 8 + step * 10;
    let grey = Rgb {
        r: level,
        g: level,
        b: level,
    };

    if distance(colour, grey) < distance(colour, cube) {
        232 + step
    } else {
        16 + 36 * ri + 6 * gi + bi
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::caps::{NotifyEscape, TermEnv};
    use std::collections::BTreeMap;

    /// The shipped default, and a second skin distinct from it for the tests below that
    /// need one.
    const DARK: &str = "catppuccin-mocha";
    const OTHER: &str = "nord";

    fn caps(depth: ColorDepth) -> Capabilities {
        Capabilities {
            color: depth,
            hyperlinks: false,
            notify: NotifyEscape::None,
            focus_events: true,
            multiplexed: false,
            over_ssh: false,
        }
    }

    fn theme(skin: &str, depth: ColorDepth) -> Theme {
        Theme::builtin(skin, false, &caps(depth))
    }

    fn ascii_theme() -> Theme {
        Theme::builtin(DARK, true, &caps(ColorDepth::TrueColor))
    }

    fn skin(name: &str) -> Skin {
        Skin {
            name: name.to_owned(),
            colors: BTreeMap::new(),
        }
    }

    const ALL_ROLES: [Role; 13] = [
        Role::Normal,
        Role::Dim,
        Role::Header,
        Role::ColumnHeader,
        Role::Border,
        Role::Selection,
        Role::Success,
        Role::Failure,
        Role::Pending,
        Role::Warning,
        Role::Added,
        Role::Removed,
        Role::Accent,
    ];

    const ALL_STATUSES: [PipelineStatus; 11] = [
        PipelineStatus::Created,
        PipelineStatus::WaitingForResource,
        PipelineStatus::Preparing,
        PipelineStatus::Pending,
        PipelineStatus::Running,
        PipelineStatus::Success,
        PipelineStatus::Failed,
        PipelineStatus::Canceled,
        PipelineStatus::Skipped,
        PipelineStatus::Manual,
        PipelineStatus::Scheduled,
    ];

    #[test]
    fn a_named_skin_is_used_as_named() {
        let named = Theme::resolve(&skin(OTHER), false, &caps(ColorDepth::TrueColor));
        assert_eq!(named.skin(), OTHER);
    }

    /// There is no light skin left to fall back to, so `auto` always means the shipped
    /// dark default.
    #[test]
    fn auto_resolves_to_the_dark_default() {
        let theme = Theme::resolve(&skin("auto"), false, &caps(ColorDepth::TrueColor));
        assert_eq!(theme.skin(), DARK);
    }

    /// The shipped default, and the one thing a user sees before they have configured
    /// anything.
    #[test]
    fn the_default_configuration_is_catppuccin_mocha() {
        let theme = Theme::resolve(&Skin::default(), false, &caps(ColorDepth::TrueColor));
        assert_eq!(
            theme.skin(),
            DARK,
            "the shipped default is not auto-detected"
        );
    }

    #[test]
    fn aliases_and_letter_case_resolve_to_the_canonical_skin() {
        for name in ["mocha", "MOCHA", " Catppuccin-Mocha "] {
            assert_eq!(theme(name, ColorDepth::TrueColor).skin(), DARK, "{name}");
        }
    }

    /// The glyph set is a font question and the palette is a colour question; a terminal
    /// can need one without the other.
    #[test]
    fn ascii_is_orthogonal_to_the_skin() {
        let ascii = Theme::builtin(OTHER, true, &caps(ColorDepth::TrueColor));

        assert!(ascii.is_ascii());
        assert_eq!(ascii.skin(), OTHER);
        assert!(!theme(OTHER, ColorDepth::TrueColor).is_ascii());
    }

    #[test]
    fn every_role_resolves_at_every_colour_depth() {
        for depth in [
            ColorDepth::TrueColor,
            ColorDepth::Indexed256,
            ColorDepth::Ansi16,
        ] {
            for name in skins::BUILTIN_NAMES {
                let theme = theme(name, depth);
                for role in ALL_ROLES {
                    assert!(
                        theme.colour(role).is_some(),
                        "{role:?} at {depth:?} on {name}"
                    );
                }
            }
        }
    }

    /// A terminal with no colour must still distinguish the roles that matter, through
    /// modifiers rather than through colour.
    #[test]
    fn a_colourless_terminal_still_distinguishes_the_key_roles() {
        let theme = theme(DARK, ColorDepth::None);

        for role in ALL_ROLES {
            assert!(
                theme.colour(role).is_none(),
                "{role:?} should have no colour"
            );
        }
        assert!(
            theme
                .style(Role::Selection)
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        assert!(theme.style(Role::Dim).add_modifier.contains(Modifier::DIM));
        assert!(
            theme
                .style(Role::Header)
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert!(
            theme
                .style(Role::ColumnHeader)
                .add_modifier
                .contains(Modifier::BOLD),
            "with no colour available, ColumnHeader falls back to bold"
        );
    }

    #[test]
    fn the_column_header_is_not_bold_when_the_terminal_has_colour() {
        let theme = theme(DARK, ColorDepth::TrueColor);
        assert!(
            !theme
                .style(Role::ColumnHeader)
                .add_modifier
                .contains(Modifier::BOLD)
        );
    }

    /// The bug a skin system makes easy: a role drawn in a colour the same brightness as
    /// the background it sits on. Absolute lightness says nothing here — a light skin's
    /// yellow is bright and still perfectly readable on white — so the test is the gap.
    #[test]
    fn every_role_contrasts_with_the_skin_it_is_drawn_on() {
        const MIN_GAP: f32 = 40.0;

        for name in skins::BUILTIN_NAMES {
            let theme = theme(name, ColorDepth::TrueColor);
            let base = theme.palette.base.luma();

            for role in ALL_ROLES {
                // The border is deliberately below text contrast, and is checked on its
                // own terms below.
                if role == Role::Border {
                    continue;
                }
                let gap = (theme.swatch(role).luma() - base).abs();
                assert!(
                    gap >= MIN_GAP,
                    "{name}: {role:?} is {gap:.0} from the background it is drawn on"
                );
            }
        }
    }

    /// Orientation, not information — but a border nobody can see is not orientation
    /// either.
    #[test]
    fn the_border_is_visible_and_still_below_text_contrast() {
        for name in skins::BUILTIN_NAMES {
            let theme = theme(name, ColorDepth::TrueColor);
            let from_base = |role| (theme.swatch(role).luma() - theme.palette.base.luma()).abs();

            assert!(from_base(Role::Border) >= 10.0, "{name}: invisible border");
            assert!(
                from_base(Role::Border) < from_base(Role::Normal),
                "{name}: the border competes with the text"
            );
        }
    }

    /// The highlight has to stay behind the text: a selection background as bright as the
    /// foreground roles is a row nobody can read.
    #[test]
    fn the_selection_highlight_sits_on_the_same_side_as_its_skin() {
        for name in skins::BUILTIN_NAMES {
            let theme = theme(name, ColorDepth::TrueColor);
            let Some(Color::Rgb(r, g, b)) = theme.selection_background() else {
                panic!("{name} should highlight with an RGB colour");
            };

            assert_eq!(
                Rgb { r, g, b }.is_dark(),
                theme.palette.base.is_dark(),
                "{name}: the highlight fights its background"
            );
        }
    }

    /// A soft highlight where the terminal has the colours for one, reverse
    /// video where it does not — and a gutter marker either way, so the cursor survives a
    /// terminal with weak reverse video.
    #[test]
    fn selection_is_visible_at_every_depth() {
        for depth in [ColorDepth::TrueColor, ColorDepth::Indexed256] {
            for name in [DARK, OTHER] {
                let style = theme(name, depth).style(Role::Selection);
                assert!(style.bg.is_some(), "no highlight at {depth:?} on {name}");
                assert!(
                    !style.add_modifier.contains(Modifier::REVERSED),
                    "a highlight and reverse video at {depth:?} cancel out"
                );
            }
        }

        for depth in [ColorDepth::Ansi16, ColorDepth::None] {
            let style = theme(DARK, depth).style(Role::Selection);
            assert!(
                style.add_modifier.contains(Modifier::REVERSED),
                "selection not reversed at {depth:?}"
            );
            assert!(style.bg.is_none());
        }
    }

    #[test]
    fn the_ansi_palette_uses_named_colours_only() {
        let theme = theme(DARK, ColorDepth::Ansi16);

        for role in ALL_ROLES {
            match theme.colour(role) {
                Some(Color::Rgb(..)) | Some(Color::Indexed(..)) => {
                    panic!("{role:?} used a high-colour value at 16-colour depth")
                }
                _ => {}
            }
        }
    }

    /// Two skins that differ in truecolor must still differ at 256 colours, or the
    /// picker does nothing on half the terminals in use.
    #[test]
    fn a_skin_change_is_visible_at_256_colours() {
        let dark = theme(DARK, ColorDepth::Indexed256);
        let other = theme(OTHER, ColorDepth::Indexed256);

        let differing = ALL_ROLES
            .iter()
            .filter(|role| dark.colour(**role) != other.colour(**role))
            .count();
        assert!(differing >= 8, "only {differing} roles differ");
    }

    /// Near-neutral surfaces belong on the grey ramp; snapping them into the colour cube
    /// is what gives a 256-colour terminal a tinted panel border.
    #[test]
    fn the_256_colour_conversion_uses_the_grey_ramp_and_the_cube() {
        assert_eq!(
            nearest_256(Rgb { r: 0, g: 0, b: 0 }),
            16,
            "the cube's corner"
        );
        assert_eq!(
            nearest_256(Rgb {
                r: 255,
                g: 255,
                b: 255
            }),
            231,
            "and its opposite"
        );

        let grey = nearest_256(Rgb {
            r: 0x4a,
            g: 0x4a,
            b: 0x4a,
        });
        assert!(
            (232..=255).contains(&grey),
            "{grey} is not on the grey ramp"
        );

        let red = nearest_256(Rgb { r: 215, g: 0, b: 0 });
        assert_eq!(red, 160);
    }

    #[test]
    fn an_override_replaces_one_swatch_and_survives_a_skin_change() {
        let mut colors = BTreeMap::new();
        colors.insert("red".to_owned(), "#ff0000".to_owned());
        let configured = Skin {
            name: DARK.to_owned(),
            colors,
        };

        let theme = Theme::resolve(&configured, false, &caps(ColorDepth::TrueColor));
        assert_eq!(theme.colour(Role::Failure), Some(Color::Rgb(0xff, 0, 0)));
        assert_eq!(
            theme.colour(Role::Success),
            Some(truecolor(skins::palette(DARK).unwrap().green)),
            "the rest of the skin is untouched"
        );

        let switched = theme.with_skin("nord");
        assert_eq!(switched.skin(), "nord");
        assert_eq!(
            switched.colour(Role::Failure),
            Some(Color::Rgb(0xff, 0, 0)),
            "the user's tweak must not be dropped by the picker"
        );
        assert_eq!(switched.is_ascii(), theme.is_ascii());
        assert_eq!(switched.depth(), theme.depth());
    }

    #[test]
    fn an_unknown_skin_is_rejected_and_lists_the_alternatives() {
        let err = check(&skin("chartreuse")).unwrap_err().to_string();

        assert!(err.contains("chartreuse"), "{err}");
        assert!(err.contains("catppuccin-mocha"), "{err}");
        assert!(err.contains("auto"), "{err}");
    }

    #[test]
    fn auto_aliases_and_built_in_names_all_pass_the_check() {
        for name in skins::BUILTIN_NAMES {
            check(&skin(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
        }
        check(&skin("auto")).unwrap();
        check(&skin("mocha")).unwrap();
        check(&Skin::default()).unwrap();
    }

    #[test]
    fn a_bad_swatch_override_names_the_offending_key() {
        let with = |key: &str, value: &str| {
            let mut colors = BTreeMap::new();
            colors.insert(key.to_owned(), value.to_owned());
            check(&Skin {
                name: DARK.to_owned(),
                colors,
            })
            .unwrap_err()
            .to_string()
        };

        let unknown = with("chartreuse", "#ff0000");
        assert!(unknown.contains("chartreuse"), "{unknown}");
        assert!(unknown.contains("rosewater"), "should list them: {unknown}");

        let malformed = with("red", "reddish");
        assert!(malformed.contains("skin.colors.red"), "{malformed}");
        assert!(malformed.contains("#rrggbb"), "{malformed}");
    }

    /// The glyph for each pipeline status.
    #[test]
    fn pipeline_glyphs_match_the_spec_table() {
        let theme = theme(DARK, ColorDepth::TrueColor);

        assert_eq!(theme.pipeline(Some(&PipelineStatus::Success)).0, "✔");
        assert_eq!(theme.pipeline(Some(&PipelineStatus::Failed)).0, "✘");
        assert_eq!(theme.pipeline(Some(&PipelineStatus::Running)).0, "●");
        assert_eq!(theme.pipeline(Some(&PipelineStatus::Pending)).0, "◌");
        assert_eq!(theme.pipeline(Some(&PipelineStatus::Canceled)).0, "⊘");
        assert_eq!(theme.pipeline(Some(&PipelineStatus::Skipped)).0, "»");
        assert_eq!(theme.pipeline(Some(&PipelineStatus::Manual)).0, "⏸");
        assert_eq!(theme.pipeline(None).0, " ", "no pipeline is blank");
    }

    #[test]
    fn pipeline_roles_carry_the_documented_meaning() {
        let theme = theme(DARK, ColorDepth::TrueColor);

        assert_eq!(
            theme.pipeline(Some(&PipelineStatus::Success)).1,
            Role::Success
        );
        assert_eq!(
            theme.pipeline(Some(&PipelineStatus::Failed)).1,
            Role::Failure
        );
        assert_eq!(
            theme.pipeline(Some(&PipelineStatus::Running)).1,
            Role::Pending
        );
        assert_eq!(theme.pipeline(Some(&PipelineStatus::Skipped)).1, Role::Dim);
    }

    /// The ascii glyphs exist for fonts without these characters, so every glyph has to
    /// be substituted — one that is missed is the one that breaks the row.
    #[test]
    fn every_glyph_is_ascii_in_the_ascii_glyph_set() {
        let theme = ascii_theme();

        let mut glyphs: Vec<&str> = ALL_STATUSES
            .iter()
            .map(|status| theme.pipeline(Some(status)).0)
            .collect();
        glyphs.push(theme.pipeline(Some(&PipelineStatus::Unknown("x".into()))).0);
        glyphs.push(theme.pipeline(None).0);
        glyphs.push(theme.selection_marker());
        glyphs.push(theme.new_marker());
        glyphs.push(theme.sort_arrow(true));
        glyphs.push(theme.sort_arrow(false));
        glyphs.push(theme.separator());
        glyphs.push(theme.tab_divider());
        glyphs.push(theme.ellipsis());
        glyphs.extend(theme.spinner_frames());

        let border = theme.border_set();
        glyphs.extend([
            border.top_left,
            border.top_right,
            border.bottom_left,
            border.bottom_right,
            border.vertical_left,
            border.vertical_right,
            border.horizontal_top,
            border.horizontal_bottom,
        ]);

        for glyph in glyphs {
            assert!(
                glyph.is_ascii(),
                "`{glyph}` is not ASCII in the ascii glyph set"
            );
        }
    }

    /// Each status needs a distinguishable cell, or the column stops carrying
    /// information at a glance.
    #[test]
    fn pipeline_glyphs_are_visually_distinct() {
        for theme in [theme(DARK, ColorDepth::TrueColor), ascii_theme()] {
            let mut seen: Vec<&str> = Vec::new();

            for status in ALL_STATUSES {
                let (glyph, _) = theme.pipeline(Some(&status));
                // Statuses that share a documented glyph are grouped deliberately.
                if !seen.contains(&glyph) {
                    seen.push(glyph);
                }
            }
            assert!(
                seen.len() >= 6,
                "collapsed into {} glyphs, ascii = {}",
                seen.len(),
                theme.is_ascii()
            );
        }
    }

    /// An unknown status still gets a cell, so the column does not misreport it as "no
    /// pipeline" — which is what a blank would mean.
    #[test]
    fn an_unknown_status_is_distinct_from_no_pipeline() {
        let theme = theme(DARK, ColorDepth::TrueColor);
        let unknown = theme
            .pipeline(Some(&PipelineStatus::Unknown("NEW".into())))
            .0;

        assert_ne!(unknown, theme.pipeline(None).0);
    }

    #[test]
    fn every_glyph_occupies_one_cell_in_the_unicode_theme() {
        let theme = theme(DARK, ColorDepth::TrueColor);

        for status in ALL_STATUSES {
            let (glyph, _) = theme.pipeline(Some(&status));
            assert_eq!(
                glyph.chars().count(),
                1,
                "`{glyph}` is more than one character"
            );
        }
        assert_eq!(theme.selection_marker().chars().count(), 1);
    }

    /// No literal colour or glyph outside the theme modules. Inlining one works for
    /// whoever wrote it and rots silently for every other terminal and every other skin.
    #[test]
    fn no_colour_or_glyph_literals_escape_this_module() {
        // Every rendering module belongs here. A new one that is not listed is not
        // covered, so add it when you add the module.
        let sources = [
            ("ui/columns.rs", include_str!("columns.rs")),
            ("ui/layout.rs", include_str!("layout.rs")),
            ("ui/mod.rs", include_str!("mod.rs")),
            ("ui/popup.rs", include_str!("popup.rs")),
            ("ui/statusbar.rs", include_str!("statusbar.rs")),
            ("ui/tabbar.rs", include_str!("tabbar.rs")),
            ("ui/table.rs", include_str!("table.rs")),
        ];

        for (name, source) in sources {
            // Rendering code only. A test may legitimately pass an ellipsis to the
            // truncation helper; what must not happen is a glyph baked into a widget.
            let code_only = source.split("#[cfg(test)]").next().unwrap_or(source);

            for (number, line) in code_only.lines().enumerate() {
                let code = line.split("//").next().unwrap_or("");
                for forbidden in ["Color::Rgb", "Color::Indexed", "Modifier::"] {
                    assert!(
                        !code.contains(forbidden),
                        "{name}:{} uses `{forbidden}` directly; go through the theme",
                        number + 1
                    );
                }
                // `·` and `—` are here because they were inlined into status-bar
                // messages once: punctuation in a sentence does not look like a glyph,
                // and the ascii terminal is the only place it shows.
                for glyph in [
                    '✔', '✘', '✓', '●', '◌', '⊘', '»', '⏸', '▎', '…', '·', '—', '↑', '↓', '│', '─',
                    '╭', '╮', '╰', '╯',
                ] {
                    assert!(
                        !code.contains(glyph),
                        "{name}:{} inlines the glyph `{glyph}`; go through the theme",
                        number + 1
                    );
                }
            }
        }
    }

    #[test]
    fn separators_and_arrows_differ_between_glyph_sets() {
        let unicode = theme(DARK, ColorDepth::TrueColor);
        let ascii = ascii_theme();

        assert_ne!(unicode.sort_arrow(true), ascii.sort_arrow(true));
        assert_ne!(unicode.sort_arrow(true), unicode.sort_arrow(false));
        assert_ne!(unicode.separator(), ascii.separator());
        assert_ne!(unicode.ellipsis(), ascii.ellipsis());
    }

    #[test]
    fn the_spinner_has_frames_in_both_glyph_sets() {
        for theme in [theme(DARK, ColorDepth::TrueColor), ascii_theme()] {
            let frames = theme.spinner_frames();
            assert!(frames.len() >= 4, "spinner is too short");
            assert!(frames.iter().all(|f| f.chars().count() == 1));
        }
    }

    #[test]
    fn the_theme_reports_the_detected_depth() {
        let theme = theme(DARK, ColorDepth::Indexed256);
        assert_eq!(theme.depth(), ColorDepth::Indexed256);
    }

    #[test]
    fn term_env_detection_feeds_the_theme() {
        let env = TermEnv {
            term: Some("xterm-kitty".to_owned()),
            colorterm: Some("truecolor".to_owned()),
            ..TermEnv::default()
        };
        let theme = Theme::resolve(&skin("auto"), false, &Capabilities::detect(&env));

        assert_eq!(theme.depth(), ColorDepth::TrueColor);
    }
}
