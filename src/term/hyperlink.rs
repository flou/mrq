//! OSC 8 hyperlinks.
//!
//! Purely additive and gated on
//! [`Capabilities::hyperlinks`](crate::term::caps::Capabilities::hyperlinks): a terminal
//! that does not understand OSC 8 prints the escape as visible text, which on a table
//! means garbage across every title cell rather than a missing feature.
//!
//! The escapes are placed into the render buffer by [`crate::ui::table`]; they are built
//! here because every raw escape sequence in the program is built in `term`.

/// Begin a hyperlink to `url`, tagged with `id`. Terminated by [`CLOSE`].
///
/// `id` disambiguates one row's link from its neighbours. Per the OSC 8 spec, a terminal
/// is free to treat two hyperlinks that carry no `id` (or the same one) as a single link
/// broken across lines — hovering or clicking one can highlight or activate the other. A
/// table draws one link per row, back to back down the screen, which is exactly the
/// layout that triggers it: without a distinguishing `id`, some terminals visually merge
/// adjacent rows' links, which reads as a hyperlink spanning multiple lines.
pub fn open(id: &str, url: &str) -> String {
    format!("\x1b]8;id={};{}\x1b\\", sanitize_id(id), sanitize_url(url))
}

/// End the hyperlink most recently opened.
pub const CLOSE: &str = "\x1b]8;;\x1b\\";

/// Strip what could end the sequence early or start another one.
///
/// The URL comes from the instance, and an `ESC` or `BEL` inside it would close the OSC
/// and leave the remainder to be interpreted as terminal commands. Newlines would break
/// the row. Nothing legal in a URL is removed — the characters taken out cannot appear in
/// one unescaped.
fn sanitize_url(url: &str) -> String {
    url.chars().filter(|c| !c.is_control()).collect()
}

/// Strip what [`sanitize_url`] strips, plus `;` and `:`.
///
/// `id` sits inside the OSC 8 parameter list, not the URI: `;` separates that list from
/// the URI itself, and `:` would separate it from further `key=value` parameters. Either
/// one inside `id` lets it smuggle in a bogus parameter or run straight into the URI —
/// characters that are perfectly legal in a URL but not here.
fn sanitize_id(id: &str) -> String {
    sanitize_url(id)
        .chars()
        .filter(|c| !matches!(c, ';' | ':'))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_link_is_an_osc_8_pair() {
        assert_eq!(
            open("482", "https://gitlab.example.com/a/b/-/merge_requests/1"),
            "\x1b]8;id=482;https://gitlab.example.com/a/b/-/merge_requests/1\x1b\\"
        );
        assert_eq!(CLOSE, "\x1b]8;;\x1b\\");
    }

    /// An `ESC` in the URL would close the OSC and leave the rest to be run as terminal
    /// commands.
    #[test]
    fn control_characters_cannot_escape_the_sequence() {
        let hostile = open("482", "https://x/\x1b]0;pwned\x07\nmore");

        assert!(
            !hostile["\x1b]8;id=482;".len()..hostile.len() - 2].contains('\x1b'),
            "{hostile:?}"
        );
        assert!(!hostile.contains('\x07'), "{hostile:?}");
        assert!(!hostile.contains('\n'), "{hostile:?}");
    }

    /// Query strings and fragments are ordinary URL syntax and must survive.
    #[test]
    fn ordinary_url_punctuation_survives() {
        let url = "https://gitlab.example.com/a/b/-/merge_requests/1?tab=diffs#note_5";
        assert!(open("482", url).contains(url));
    }

    /// `:` and `;` are OSC 8 parameter delimiters; a `gid://gitlab/MergeRequest/1`-shaped
    /// id must not be able to inject a second parameter or close the parameter list early.
    #[test]
    fn id_delimiters_cannot_smuggle_in_a_parameter() {
        let link = open(
            "gid://gitlab/MergeRequest/1",
            "https://gitlab.example.com/mr/1",
        );

        assert_eq!(
            link,
            "\x1b]8;id=gid//gitlab/MergeRequest/1;https://gitlab.example.com/mr/1\x1b\\"
        );
    }

    /// Two rows' links must carry distinct ids, or a terminal may treat them as one link
    /// spanning both rows.
    #[test]
    fn different_rows_get_different_ids() {
        let a = open("1", "https://gitlab.example.com/mr/1");
        let b = open("2", "https://gitlab.example.com/mr/2");
        assert_ne!(a, b);
    }
}
