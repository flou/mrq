//! The OSC 0 screen title: the BEL-terminated escape, the `mrq — {filter}` format it is
//! fed, and the sanitisation that keeps a hostile MR name from cutting the escape short.

use std::fmt::Write as _;

/// The escape that sets the title: `ESC ] 0 ; {text} BEL`, stripped of control
/// characters first. An `ESC` or `BEL` inside the text would end the sequence early and
/// leave the remainder to be read as terminal commands, and any other control character
/// would print garbage into the user's screen.
pub fn escape(text: &str) -> String {
    let sanitized: String = text.chars().filter(|c| !c.is_control()).collect();
    format!("\x1b]0;{sanitized}\x07")
}

/// `mrq — {filter} ({count}{, N new})`. The `, N new` clause appears only when some rows
/// arrived in the last refresh; a bare count is the resting state and saying `0 new` is
/// noise.
pub fn compose(filter: &str, count: usize, new: usize) -> String {
    let mut out = format!("mrq \u{2014} {filter} ({count}");
    if new > 0 {
        // A `String` write never fails; nothing to propagate.
        let _ = write!(out, ", {new} new");
    }
    out.push(')');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_follows_the_documented_shape() {
        assert_eq!(compose("review", 7, 0), "mrq \u{2014} review (7)");
        assert_eq!(compose("review", 7, 2), "mrq \u{2014} review (7, 2 new)");
    }

    #[test]
    fn compose_omits_an_empty_new_clause() {
        assert_eq!(compose("all", 0, 0), "mrq \u{2014} all (0)");
    }

    #[test]
    fn the_escape_wraps_and_terminates() {
        assert_eq!(
            escape("mrq \u{2014} all (12)"),
            "\x1b]0;mrq \u{2014} all (12)\x07"
        );
    }

    #[test]
    fn control_characters_cannot_shorten_the_sequence() {
        let hostile = "mrq \u{2014} \x1b]2;owned\x07({}\x07more";
        let escape = escape(hostile);
        assert!(
            !escape[1..].contains('\u{1b}'),
            "the only ESC allowed is the opener: {escape:?}"
        );
        assert!(
            !escape[..escape.len() - 1].contains('\u{07}'),
            "the only BEL allowed is the terminator: {escape:?}"
        );
    }
}
