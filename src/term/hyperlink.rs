//! OSC 8 hyperlinks.
//!
//! Purely additive and gated on
//! [`Capabilities::hyperlinks`](crate::term::caps::Capabilities::hyperlinks): a terminal
//! that does not understand OSC 8 prints the escape as visible text, which on a table
//! means garbage across every title cell rather than a missing feature.
//!
//! The escapes are placed into the render buffer by [`crate::ui::table`]; they are built
//! here because every raw escape sequence in the program is built in `term`.

/// Begin a hyperlink to `url`. Terminated by [`CLOSE`].
pub fn open(url: &str) -> String {
    format!("\x1b]8;;{}\x1b\\", sanitize(url))
}

/// End the hyperlink most recently opened.
pub const CLOSE: &str = "\x1b]8;;\x1b\\";

/// Strip what could end the sequence early or start another one.
///
/// The URL comes from the instance, and an `ESC` or `BEL` inside it would close the OSC
/// and leave the remainder to be interpreted as terminal commands. Newlines would break
/// the row. Nothing legal in a URL is removed — the characters taken out cannot appear in
/// one unescaped.
fn sanitize(url: &str) -> String {
    url.chars().filter(|c| !c.is_control()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_link_is_an_osc_8_pair() {
        assert_eq!(
            open("https://gitlab.example.com/a/b/-/merge_requests/1"),
            "\x1b]8;;https://gitlab.example.com/a/b/-/merge_requests/1\x1b\\"
        );
        assert_eq!(CLOSE, "\x1b]8;;\x1b\\");
    }

    /// An `ESC` in the URL would close the OSC and leave the rest to be run as terminal
    /// commands.
    #[test]
    fn control_characters_cannot_escape_the_sequence() {
        let hostile = open("https://x/\x1b]0;pwned\x07\nmore");

        assert!(
            !hostile[2..hostile.len() - 2].contains('\x1b'),
            "{hostile:?}"
        );
        assert!(!hostile.contains('\x07'), "{hostile:?}");
        assert!(!hostile.contains('\n'), "{hostile:?}");
    }

    /// Query strings and fragments are ordinary URL syntax and must survive.
    #[test]
    fn ordinary_url_punctuation_survives() {
        let url = "https://gitlab.example.com/a/b/-/merge_requests/1?tab=diffs#note_5";
        assert!(open(url).contains(url));
    }
}
