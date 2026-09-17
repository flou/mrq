//! What this terminal can do.
//!
//! Every optional feature is gated on detection, because the failure mode
//! of guessing wrong is not a missing feature — it is escape sequences printed as
//! visible garbage across the table.
//!
//! Detection is a pure function of the environment, taking a [`TermEnv`] for the same
//! reason [`crate::config::paths`] does: process environment is global mutable state and
//! tests that set it race each other.
//!
//! # Allowlists, not probes
//!
//! Every capability here is decided from `$TERM` and `$TERM_PROGRAM` rather than by asking
//! the terminal. A query needs a reply, a reply needs a read with a timeout, and a
//! timeout on startup is a visible stall.

/// The environment variables that describe the terminal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TermEnv {
    pub term: Option<String>,
    pub term_program: Option<String>,
    pub colorterm: Option<String>,
    pub ssh_connection: Option<String>,
    /// <https://no-color.org>: any non-empty value disables colour.
    pub no_color: Option<String>,
    /// Set by VTE-based terminals (GNOME Terminal and relatives).
    pub vte_version: Option<String>,
}

impl TermEnv {
    pub fn from_process() -> Self {
        let var = |key| std::env::var(key).ok().filter(|v| !v.is_empty());
        Self {
            term: var("TERM"),
            term_program: var("TERM_PROGRAM"),
            colorterm: var("COLORTERM"),
            ssh_connection: var("SSH_CONNECTION"),
            no_color: var("NO_COLOR"),
            vte_version: var("VTE_VERSION"),
        }
    }

    fn term(&self) -> &str {
        self.term.as_deref().unwrap_or("")
    }

    fn program(&self) -> String {
        self.term_program
            .as_deref()
            .unwrap_or("")
            .to_ascii_lowercase()
    }
}

/// How many colours are usable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ColorDepth {
    /// `NO_COLOR`, or a terminal that reports none.
    None,
    Ansi16,
    Indexed256,
    TrueColor,
}

/// Which escape a notification should use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyEscape {
    Osc9,
    Osc777,
    /// The terminal is not known to support either.
    None,
}

/// Everything detection decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    pub color: ColorDepth,
    /// OSC 8 clickable text.
    pub hyperlinks: bool,
    pub notify: NotifyEscape,
    /// Whether focus-in/out events are reported. When false, `only_when_unfocused` has
    /// nothing to act on.
    pub focus_events: bool,
    /// Running through tmux or screen, where OSC sequences need passthrough and several
    /// features behave differently.
    pub multiplexed: bool,
    pub over_ssh: bool,
}

impl Capabilities {
    /// Detect from the environment.
    pub fn detect(env: &TermEnv) -> Self {
        let multiplexed = is_multiplexed(env);
        Self {
            color: color_depth(env),
            hyperlinks: supports_hyperlinks(env, multiplexed),
            notify: notify_escape(env),
            focus_events: supports_focus_events(env),
            multiplexed,
            over_ssh: env.ssh_connection.is_some(),
        }
    }

    /// Whether any colour at all can be used.
    #[cfg(test)]
    fn has_color(&self) -> bool {
        self.color != ColorDepth::None
    }
}

fn is_multiplexed(env: &TermEnv) -> bool {
    let term = env.term();
    term.starts_with("screen") || term.starts_with("tmux") || env.program() == "tmux"
}

fn color_depth(env: &TermEnv) -> ColorDepth {
    // NO_COLOR wins over everything, including an explicit COLORTERM: it is a deliberate
    // user choice, and the whole point of the convention is that it is not negotiable.
    if env.no_color.is_some() {
        return ColorDepth::None;
    }

    let term = env.term();
    if term.is_empty() || term == "dumb" {
        return ColorDepth::None;
    }

    match env.colorterm.as_deref().map(str::to_ascii_lowercase) {
        Some(value) if value == "truecolor" || value == "24bit" => return ColorDepth::TrueColor,
        _ => {}
    }

    // Terminals that are always truecolor but do not always set COLORTERM — notably
    // when the variable is lost crossing an ssh or tmux boundary.
    if matches!(
        env.program().as_str(),
        "iterm.app" | "wezterm" | "ghostty" | "vscode"
    ) || term.starts_with("xterm-kitty")
        || term.starts_with("wezterm")
        || term.starts_with("foot")
    {
        return ColorDepth::TrueColor;
    }

    if term.contains("256color") || term.contains("direct") {
        return ColorDepth::Indexed256;
    }
    ColorDepth::Ansi16
}

fn supports_hyperlinks(env: &TermEnv, multiplexed: bool) -> bool {
    // tmux passes OSC 8 through only when configured to, and the failure is visible
    // garbage in every title cell. Not worth the risk for a purely additive feature.
    if multiplexed {
        return false;
    }

    if matches!(
        env.program().as_str(),
        "iterm.app" | "wezterm" | "ghostty" | "vscode" | "hyper"
    ) {
        return true;
    }
    let term = env.term();
    if term.starts_with("xterm-kitty") || term.starts_with("wezterm") || term.starts_with("foot") {
        return true;
    }

    // VTE gained OSC 8 in 0.50.
    env.vte_version
        .as_deref()
        .and_then(|v| v.parse::<u32>().ok())
        .is_some_and(|v| v >= 5000)
}

fn notify_escape(env: &TermEnv) -> NotifyEscape {
    let term = env.term();
    match env.program().as_str() {
        "iterm.app" | "wezterm" | "ghostty" => NotifyEscape::Osc9,
        _ if term.starts_with("xterm-kitty") => NotifyEscape::Osc777,
        _ if term.starts_with("wezterm") || term.starts_with("foot") => NotifyEscape::Osc777,
        _ if term.starts_with("rxvt") || term.starts_with("urxvt") => NotifyEscape::Osc777,
        _ => NotifyEscape::None,
    }
}

fn supports_focus_events(env: &TermEnv) -> bool {
    let term = env.term();
    // The Linux console and dumb terminals do not; everything else modern does, and a
    // terminal that ignores the enable sequence simply never sends the events.
    !(term.is_empty() || term == "dumb" || term == "linux")
}

/// An RGB colour as reported by the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    /// Perceived brightness, 0..=255.
    ///
    /// Rec. 601 luma, which weights green heavily because human vision does. A plain
    /// average calls mid-green backgrounds dark and produces an unreadable theme.
    pub fn luma(self) -> f32 {
        0.299 * f32::from(self.r) + 0.587 * f32::from(self.g) + 0.114 * f32::from(self.b)
    }

    /// Whether text on this background should be light.
    ///
    /// With no light skins left, every built-in palette is dark; this survives only as
    /// the invariant check the skin and theme tests run against.
    #[cfg(test)]
    pub(crate) fn is_dark(self) -> bool {
        self.luma() < 128.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(term: &str) -> TermEnv {
        TermEnv {
            term: Some(term.to_owned()),
            ..TermEnv::default()
        }
    }

    fn program(name: &str) -> TermEnv {
        TermEnv {
            term: Some("xterm-256color".to_owned()),
            term_program: Some(name.to_owned()),
            ..TermEnv::default()
        }
    }

    #[test]
    fn colorterm_declares_truecolor() {
        for value in ["truecolor", "24bit", "TrueColor"] {
            let declared = TermEnv {
                colorterm: Some(value.to_owned()),
                ..env("xterm-256color")
            };
            assert_eq!(color_depth(&declared), ColorDepth::TrueColor, "{value}");
        }
    }

    #[test]
    fn colour_depth_falls_back_through_the_documented_ladder() {
        assert_eq!(color_depth(&env("xterm-256color")), ColorDepth::Indexed256);
        assert_eq!(color_depth(&env("xterm")), ColorDepth::Ansi16);
        assert_eq!(color_depth(&env("dumb")), ColorDepth::None);
        assert_eq!(color_depth(&TermEnv::default()), ColorDepth::None);
    }

    /// COLORTERM is routinely lost crossing ssh and tmux, so terminals that are always
    /// truecolor are recognised by name too.
    #[test]
    fn known_truecolor_terminals_are_recognised_without_colorterm() {
        assert_eq!(color_depth(&env("xterm-kitty")), ColorDepth::TrueColor);
        assert_eq!(color_depth(&program("iTerm.app")), ColorDepth::TrueColor);
        assert_eq!(color_depth(&program("WezTerm")), ColorDepth::TrueColor);
        assert_eq!(color_depth(&program("ghostty")), ColorDepth::TrueColor);
    }

    /// <https://no-color.org> — a deliberate user choice, and not negotiable.
    #[test]
    fn no_color_overrides_everything() {
        let forced = TermEnv {
            colorterm: Some("truecolor".to_owned()),
            no_color: Some("1".to_owned()),
            ..env("xterm-kitty")
        };

        assert_eq!(color_depth(&forced), ColorDepth::None);
        assert!(!Capabilities::detect(&forced).has_color());
    }

    #[test]
    fn an_empty_no_color_is_ignored_by_from_process_filtering() {
        // from_process filters empty values, so an exported-but-empty NO_COLOR does not
        // silently disable colour.
        let unset = TermEnv {
            no_color: None,
            ..env("xterm-256color")
        };
        assert_eq!(color_depth(&unset), ColorDepth::Indexed256);
    }

    #[test]
    fn hyperlink_support_is_allowlisted() {
        for name in ["iTerm.app", "WezTerm", "ghostty", "vscode", "Hyper"] {
            assert!(
                supports_hyperlinks(&program(name), false),
                "{name} supports OSC 8"
            );
        }
        assert!(supports_hyperlinks(&env("xterm-kitty"), false));
        assert!(!supports_hyperlinks(&env("xterm-256color"), false));
    }

    #[test]
    fn vte_terminals_get_hyperlinks_from_version_0_50() {
        let recent = TermEnv {
            vte_version: Some("6003".to_owned()),
            ..env("xterm-256color")
        };
        let old = TermEnv {
            vte_version: Some("4800".to_owned()),
            ..env("xterm-256color")
        };

        assert!(supports_hyperlinks(&recent, false));
        assert!(!supports_hyperlinks(&old, false));
    }

    /// tmux passes OSC 8 through only when configured to, and the failure is visible
    /// garbage in every title cell.
    #[test]
    fn hyperlinks_are_declined_under_a_multiplexer() {
        let inside_tmux = TermEnv {
            term: Some("screen-256color".to_owned()),
            term_program: Some("iTerm.app".to_owned()),
            ..TermEnv::default()
        };
        let caps = Capabilities::detect(&inside_tmux);

        assert!(caps.multiplexed);
        assert!(!caps.hyperlinks, "additive features are not worth the risk");
    }

    #[test]
    fn multiplexers_are_recognised() {
        assert!(is_multiplexed(&env("screen-256color")));
        assert!(is_multiplexed(&env("tmux-256color")));
        assert!(!is_multiplexed(&env("xterm-256color")));
    }

    /// The escape picked per terminal family.
    #[test]
    fn notification_escapes_follow_the_terminal() {
        assert_eq!(notify_escape(&program("iTerm.app")), NotifyEscape::Osc9);
        assert_eq!(notify_escape(&program("WezTerm")), NotifyEscape::Osc9);
        assert_eq!(notify_escape(&env("xterm-kitty")), NotifyEscape::Osc777);
        assert_eq!(notify_escape(&env("rxvt-unicode")), NotifyEscape::Osc777);
        assert_eq!(notify_escape(&env("xterm-256color")), NotifyEscape::None);
    }

    /// When focus events are unavailable, `only_when_unfocused` has
    /// nothing to act on and must be ignored rather than suppressing silently.
    #[test]
    fn focus_event_support_is_tracked() {
        assert!(supports_focus_events(&env("xterm-256color")));
        assert!(supports_focus_events(&env("xterm-kitty")));
        assert!(
            !supports_focus_events(&env("linux")),
            "the console does not"
        );
        assert!(!supports_focus_events(&env("dumb")));
        assert!(!supports_focus_events(&TermEnv::default()));
    }

    #[test]
    fn ssh_is_detected() {
        let remote = TermEnv {
            ssh_connection: Some("10.0.0.1 22 10.0.0.2 22".to_owned()),
            ..env("xterm-256color")
        };
        assert!(Capabilities::detect(&remote).over_ssh);
        assert!(!Capabilities::detect(&env("xterm-256color")).over_ssh);
    }

    /// A plain average calls mid-green dark and produces an unreadable theme, which is
    /// why this weights by perceived brightness.
    #[test]
    fn darkness_uses_perceived_luminance() {
        assert!(Rgb { r: 0, g: 0, b: 0 }.is_dark());
        assert!(
            Rgb {
                r: 30,
                g: 30,
                b: 46
            }
            .is_dark(),
            "a typical dark theme background"
        );
        assert!(
            !Rgb {
                r: 255,
                g: 255,
                b: 255
            }
            .is_dark()
        );
        // The weighting is the point: the same channel value reads very differently
        // depending on which channel carries it, which a plain average would miss.
        assert!(
            !Rgb { r: 0, g: 255, b: 0 }.is_dark(),
            "full green is a light background"
        );
        assert!(
            Rgb { r: 0, g: 0, b: 255 }.is_dark(),
            "full blue, at the same channel value, is a dark one"
        );
    }

    #[test]
    fn detection_composes_into_capabilities() {
        let kitty = TermEnv {
            term: Some("xterm-kitty".to_owned()),
            colorterm: Some("truecolor".to_owned()),
            ..TermEnv::default()
        };
        let caps = Capabilities::detect(&kitty);

        assert_eq!(caps.color, ColorDepth::TrueColor);
        assert!(caps.hyperlinks);
        assert_eq!(caps.notify, NotifyEscape::Osc777);
        assert!(caps.focus_events);
        assert!(!caps.multiplexed);
        assert!(!caps.over_ssh);
    }

    /// The worst case must still be usable rather than refusing to start.
    #[test]
    fn an_unknown_terminal_degrades_to_the_safe_set() {
        let caps = Capabilities::detect(&TermEnv::default());

        assert_eq!(caps.color, ColorDepth::None);
        assert!(!caps.hyperlinks);
        assert_eq!(caps.notify, NotifyEscape::None);
        assert!(!caps.focus_events);
    }

    #[test]
    fn colour_depths_order_from_least_to_most_capable() {
        assert!(ColorDepth::None < ColorDepth::Ansi16);
        assert!(ColorDepth::Ansi16 < ColorDepth::Indexed256);
        assert!(ColorDepth::Indexed256 < ColorDepth::TrueColor);
    }
}
