//! Line-oriented markdown rendering for the details popup's description.
//!
//! A full CommonMark parser normalises away exactly what this popup wants to keep —
//! leading indentation, verbatim code and table rows — so rendering stays line-oriented
//! instead: every source line maps to one or more output lines, styled per [`Role`].

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::ui::theme::Role;

/// One styled run of text within a rendered line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub role: Role,
    pub text: String,
}

/// A rendered markdown line, in left-to-right order.
pub type StyledLine = Vec<Segment>;

fn seg(role: Role, text: impl Into<String>) -> Segment {
    Segment {
        role,
        text: text.into(),
    }
}

/// Render `text` as markdown, wrapped to `width` terminal cells.
///
/// `width == 0` disables wrapping (a line per source line, however wide).
pub fn render(text: &str, width: usize) -> Vec<StyledLine> {
    let sanitized = sanitize(text);
    if sanitized.trim().is_empty() {
        return vec![vec![seg(Role::Dim, "(no description)")]];
    }

    let mut out = Vec::new();
    let mut fence: Option<String> = None;
    let mut in_comment = false;

    for line in sanitized.split('\n') {
        let trimmed = line.trim_start();
        let indent = &line[..line.len() - trimmed.len()];

        if in_comment {
            out.push(vec![seg(Role::Dim, line)]);
            in_comment = !trimmed.contains("-->");
            continue;
        }

        if let Some(marker) = &fence {
            if trimmed.starts_with(marker.as_str()) {
                out.push(vec![seg(Role::Dim, line)]);
                fence = None;
            } else {
                out.extend(hard_wrap(Role::Code, line, width));
            }
            continue;
        }

        if let Some(marker) = fence_marker(trimmed) {
            out.push(vec![seg(Role::Dim, line)]);
            fence = Some(marker.to_owned());
            continue;
        }

        if trimmed.starts_with("<!--") {
            out.push(vec![seg(Role::Dim, line)]);
            in_comment = !trimmed.contains("-->");
            continue;
        }

        if trimmed.is_empty() {
            out.push(Vec::new());
            continue;
        }

        if is_thematic_break(trimmed) {
            out.push(vec![seg(Role::Dim, line)]);
            continue;
        }

        if heading_level(trimmed).is_some() {
            out.push(vec![seg(Role::Header, line)]);
            continue;
        }

        if trimmed.starts_with('|') {
            out.extend(hard_wrap(Role::Normal, line, width));
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix('>') {
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            let prefix = format!("{indent}> ");
            out.extend(wrap_prefixed(&prefix, Role::Dim, rest, width, Role::Dim));
            continue;
        }

        if let Some(marker_len) = list_marker_len(trimmed) {
            let prefix = format!("{indent}{}", &trimmed[..marker_len]);
            out.extend(wrap_prefixed(
                &prefix,
                Role::Marker,
                &trimmed[marker_len..],
                width,
                Role::Normal,
            ));
            continue;
        }

        out.extend(wrap_prefixed(
            indent,
            Role::Normal,
            trimmed,
            width,
            Role::Normal,
        ));
    }

    out
}

/// CRLF/CR to LF, tabs expanded to four columns (not stripped as control characters, or
/// `foo\tbar` glues into `foobar`), remaining control characters dropped so a stray escape
/// sequence in the description cannot corrupt the terminal.
fn sanitize(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\t', "    ")
        .chars()
        .filter(|c| *c == '\n' || !c.is_control())
        .collect()
}

/// The fence marker (` ``` ` or `~~~`, three or more) a fenced-code line opens, if any.
fn fence_marker(trimmed: &str) -> Option<&'static str> {
    if trimmed.starts_with("```") {
        Some("```")
    } else if trimmed.starts_with("~~~") {
        Some("~~~")
    } else {
        None
    }
}

/// `---`, `***` or `___` alone on a line (spaces allowed between the characters).
fn is_thematic_break(trimmed: &str) -> bool {
    let mut chars = trimmed.chars().filter(|c| !c.is_whitespace());
    let Some(first) = chars.next() else {
        return false;
    };
    matches!(first, '-' | '*' | '_') && chars.clone().count() >= 2 && chars.all(|c| c == first)
}

/// `#` through `######`, followed by a space or end of line.
fn heading_level(trimmed: &str) -> Option<usize> {
    let level = trimmed.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&level) {
        matches!(trimmed.as_bytes().get(level), None | Some(b' ')).then_some(level)
    } else {
        None
    }
}

/// The byte length of a list marker (`- `, `* `, `+ `, `1. `, `1) `, each optionally
/// followed by a `[ ]`/`[x]` checkbox) at the start of `trimmed`, if it opens with one.
fn list_marker_len(trimmed: &str) -> Option<usize> {
    let bytes = trimmed.as_bytes();
    let bullet_len = match bytes.first() {
        Some(b'-' | b'*' | b'+') if bytes.get(1) == Some(&b' ') => 2,
        _ => {
            let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
            match (digits, bytes.get(digits), bytes.get(digits + 1)) {
                (1..=9, Some(b'.' | b')'), Some(b' ')) => digits + 2,
                _ => return None,
            }
        }
    };

    let after = &trimmed[bullet_len..];
    let checkbox_len = matches!(after.as_bytes(), [b'[', b' ' | b'x' | b'X', b']', b' ', ..])
        .then_some(4)
        .unwrap_or(0);
    Some(bullet_len + checkbox_len)
}

/// Wrap `content` (inline-styled, `base_role` where nothing more specific applies) to
/// `width` cells, with `prefix` (styled `prefix_role`) on the first line and matching
/// blank padding on every continuation line so wrapped list items and quotes hang under
/// their own text.
fn wrap_prefixed(
    prefix: &str,
    prefix_role: Role,
    content: &str,
    width: usize,
    base_role: Role,
) -> Vec<StyledLine> {
    let indent_width = prefix.width();
    let inner_width = width.saturating_sub(indent_width);
    let segments = parse_inline(content, base_role);
    let wrapped = wrap_segments(&segments, inner_width);

    let hang = " ".repeat(indent_width);
    wrapped
        .into_iter()
        .enumerate()
        .map(|(index, mut line)| {
            let lead = if index == 0 { prefix } else { &hang };
            if !lead.is_empty() {
                line.insert(0, seg(prefix_role, lead));
            }
            line
        })
        .collect()
}

/// Hard-wrap verbatim text (fenced code, table rows) at `width` cells with no word
/// splitting on whitespace: markdown's other whitespace-preserving content.
fn hard_wrap(role: Role, line: &str, width: usize) -> Vec<StyledLine> {
    if width == 0 || line.width() <= width {
        return vec![vec![seg(role, line)]];
    }

    let mut out = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    for ch in line.chars() {
        let char_width = ch.width().unwrap_or(0);
        if current_width + char_width > width && !current.is_empty() {
            out.push(vec![seg(role, std::mem::take(&mut current))]);
            current_width = 0;
        }
        current.push(ch);
        current_width += char_width;
    }
    out.push(vec![seg(role, current)]);
    out
}

/// A single markdown inline delimiter and the role its span renders as.
struct Delimiter {
    open: &'static str,
    role: Role,
    /// Underscore emphasis only fires at a word boundary, or `snake_case` would light up.
    word_boundary: bool,
}

const DELIMITERS: [Delimiter; 4] = [
    Delimiter {
        open: "**",
        role: Role::Strong,
        word_boundary: false,
    },
    Delimiter {
        open: "__",
        role: Role::Strong,
        word_boundary: true,
    },
    Delimiter {
        open: "*",
        role: Role::Emphasis,
        word_boundary: false,
    },
    Delimiter {
        open: "_",
        role: Role::Emphasis,
        word_boundary: true,
    },
];

/// Parse one line's inline markdown (code spans, emphasis, links, bare URLs) into styled
/// segments. Plain runs take `base_role`, so a blockquote's text can default to `Dim`
/// while its inline code or links still get their own colour.
fn parse_inline(text: &str, base_role: Role) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::new();
    let mut plain = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;

    macro_rules! flush_plain {
        () => {
            if !plain.is_empty() {
                out.push(seg(base_role, std::mem::take(&mut plain)));
            }
        };
    }

    while i < chars.len() {
        if chars[i] == '`' {
            let run = chars[i..].iter().take_while(|c| **c == '`').count();
            if let Some(close) = find_run(&chars, i + run, run, '`') {
                flush_plain!();
                let content: String = chars[i + run..close].iter().collect();
                out.push(seg(Role::Code, content));
                i = close + run;
                continue;
            }
        }

        if let Some((consumed, link_text)) = parse_link(&chars, i) {
            flush_plain!();
            out.push(seg(Role::Link, link_text));
            i += consumed;
            continue;
        }

        if let Some((consumed, url)) = parse_autolink(&chars, i) {
            flush_plain!();
            out.push(seg(Role::Link, url));
            i += consumed;
            continue;
        }

        if let Some(delim) = DELIMITERS.iter().find(|d| starts_with(&chars, i, d.open)) {
            let open_len = delim.open.chars().count();
            let opens = !delim.word_boundary || is_boundary(&chars, i.checked_sub(1));
            if opens
                && let Some(close) = find_run(
                    &chars,
                    i + open_len,
                    open_len,
                    delim.open.chars().next().unwrap_or_default(),
                )
                && close > i + open_len
                && (!delim.word_boundary || is_boundary(&chars, Some(close + open_len)))
            {
                flush_plain!();
                let content: String = chars[i + open_len..close].iter().collect();
                out.push(seg(delim.role, content));
                i = close + open_len;
                continue;
            }
        }

        plain.push(chars[i]);
        i += 1;
    }
    flush_plain!();
    out
}

/// Whether `chars[i..]` starts with the literal `needle`.
fn starts_with(chars: &[char], i: usize, needle: &str) -> bool {
    needle
        .chars()
        .enumerate()
        .all(|(offset, c)| chars.get(i + offset) == Some(&c))
}

/// The index of a run of `len` consecutive `marker` characters at or after `from`, so
/// callers can find a delimiter's matching close.
fn find_run(chars: &[char], from: usize, len: usize, marker: char) -> Option<usize> {
    let mut i = from;
    while i + len <= chars.len() {
        if chars[i..i + len].iter().all(|c| *c == marker) {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Underscore emphasis only opens/closes where neither neighbour is alphanumeric, so
/// `snake_case` and `__init__` stay plain text.
fn is_boundary(chars: &[char], index: Option<usize>) -> bool {
    match index {
        None => true,
        Some(i) => !chars.get(i).is_some_and(|c| c.is_alphanumeric()),
    }
}

/// `[text](url)` or `![alt](url)`, returning the characters consumed and the text shown.
fn parse_link(chars: &[char], i: usize) -> Option<(usize, String)> {
    let start = if chars.get(i) == Some(&'!') { i + 1 } else { i };
    if chars.get(start) != Some(&'[') {
        return None;
    }
    let text_end = (start + 1..chars.len()).find(|j| chars[*j] == ']')?;
    if chars.get(text_end + 1) != Some(&'(') {
        return None;
    }
    let url_end = (text_end + 2..chars.len()).find(|j| chars[*j] == ')')?;

    let text: String = chars[start + 1..text_end].iter().collect();
    let text = if text.is_empty() {
        chars[text_end + 2..url_end].iter().collect()
    } else {
        text
    };
    Some((url_end + 1 - i, text))
}

/// `<http://...>` or a bare `http://`/`https://` run up to the next whitespace.
fn parse_autolink(chars: &[char], i: usize) -> Option<(usize, String)> {
    let bracketed = chars.get(i) == Some(&'<');
    let start = if bracketed { i + 1 } else { i };

    let scheme = ["https://", "http://"]
        .into_iter()
        .find(|s| starts_with(chars, start, s))?;
    let mut end = start + scheme.chars().count();
    while end < chars.len() && !chars[end].is_whitespace() && !(bracketed && chars[end] == '>') {
        end += 1;
    }

    let url: String = chars[start..end].iter().collect();
    if bracketed {
        (chars.get(end) == Some(&'>')).then_some((end + 1 - i, url))
    } else {
        Some((end - i, url))
    }
}

/// Word-wrap styled `segments` to `width` cells: a word never splits across output lines
/// unless it alone exceeds `width`, in which case it hard-breaks. Adjacent words that
/// share a role are merged back into one segment, so a wrapped sentence is not hundreds
/// of one-word spans.
fn wrap_segments(segments: &[Segment], width: usize) -> Vec<StyledLine> {
    let words = tokenize(segments);
    if words.is_empty() {
        return vec![Vec::new()];
    }
    // `width == 0` means "do not wrap": treated as unbounded rather than as a width no
    // word could ever fit, which would hard-break every word into single characters.
    let unbounded = width == 0;

    let mut lines = Vec::new();
    let mut line: Vec<(Role, String)> = Vec::new();
    let mut line_width = 0usize;

    for (role, word) in words {
        let word_width = word.width();

        if !unbounded && word_width > width {
            if !line.is_empty() {
                lines.push(std::mem::take(&mut line));
                line_width = 0;
            }
            for chunk in break_word(&word, width) {
                lines.push(vec![(role, chunk)]);
            }
            continue;
        }

        let needed = if line.is_empty() {
            word_width
        } else {
            line_width + 1 + word_width
        };
        if !unbounded && needed > width && !line.is_empty() {
            lines.push(std::mem::take(&mut line));
            line_width = 0;
        }
        if !line.is_empty() {
            line.push((Role::Normal, " ".to_owned()));
            line_width += 1;
        }
        line.push((role, word));
        line_width += word_width;
    }
    if !line.is_empty() {
        lines.push(line);
    }

    lines
        .into_iter()
        .map(|tokens| tokens.into_iter().fold(Vec::new(), push_merged))
        .collect()
}

/// Append `(role, text)` to `line`, merging into the last segment when the role matches.
fn push_merged(mut line: StyledLine, (role, text): (Role, String)) -> StyledLine {
    match line.last_mut() {
        Some(last) if last.role == role => last.text.push_str(&text),
        _ => line.push(seg(role, text)),
    }
    line
}

/// Split `word` into `width`-wide chunks, breaking on character boundaries.
fn break_word(word: &str, width: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    for ch in word.chars() {
        let char_width = ch.width().unwrap_or(0);
        if current_width + char_width > width && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
            current_width = 0;
        }
        current.push(ch);
        current_width += char_width;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Split styled segments on whitespace into `(role, word)` tokens, dropping the
/// whitespace itself (the wrapper re-inserts single spaces between words).
fn tokenize(segments: &[Segment]) -> Vec<(Role, String)> {
    segments
        .iter()
        .flat_map(|segment| -> Vec<(Role, String)> {
            // A styled span (code, link, bold, italic) wraps as one indivisible word, or
            // an inline code span with a space in it — `` `cargo test` `` — would split
            // across a line break and re-merge into two separate spans either side of it.
            if matches!(
                segment.role,
                Role::Code | Role::Link | Role::Strong | Role::Emphasis
            ) {
                if segment.text.is_empty() {
                    Vec::new()
                } else {
                    vec![(segment.role, segment.text.clone())]
                }
            } else {
                segment
                    .text
                    .split_whitespace()
                    .map(|word| (segment.role, word.to_owned()))
                    .collect()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(line: &StyledLine) -> String {
        line.iter().map(|s| s.text.as_str()).collect()
    }

    fn joined(lines: &[StyledLine]) -> String {
        lines.iter().map(text).collect::<Vec<_>>().join("\n")
    }

    fn roles(line: &StyledLine) -> Vec<Role> {
        line.iter().map(|s| s.role).collect()
    }

    #[test]
    fn empty_description_says_so() {
        let lines = render("   \n  ", 80);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0][0].role, Role::Dim);
        assert_eq!(lines[0][0].text, "(no description)");
    }

    #[test]
    fn a_tab_does_not_glue_words_together() {
        let lines = render("foo\tbar", 80);
        assert_eq!(joined(&lines), "foo bar");
    }

    #[test]
    fn leading_indentation_is_kept() {
        let lines = render("    indented text", 80);
        assert_eq!(text(&lines[0]), "    indented text");
    }

    #[test]
    fn a_heading_is_styled_as_header() {
        let lines = render("## Section", 80);
        assert_eq!(lines[0][0].role, Role::Header);
        assert_eq!(text(&lines[0]), "## Section");
    }

    #[test]
    fn a_hash_without_a_space_is_not_a_heading() {
        let lines = render("#no-space", 80);
        assert_ne!(lines[0][0].role, Role::Header);
    }

    #[test]
    fn fenced_code_is_verbatim_and_dims_its_fence() {
        let lines = render("```\nfn  x() {\n    1\n}\n```", 80);
        assert_eq!(lines[0][0].role, Role::Dim);
        assert_eq!(lines[4][0].role, Role::Dim);
        assert_eq!(text(&lines[1]), "fn  x() {");
        assert_eq!(lines[1][0].role, Role::Code);
        assert_eq!(text(&lines[2]), "    1");
    }

    #[test]
    fn a_multiline_html_comment_is_dimmed() {
        let lines = render("<!-- start\nhidden\nend -->\nvisible", 80);
        assert!(lines[..3].iter().all(|l| l[0].role == Role::Dim));
        assert_eq!(text(&lines[3]), "visible");
        assert_ne!(lines[3][0].role, Role::Dim);
    }

    #[test]
    fn inline_code_is_styled() {
        let lines = render("run `cargo test` now", 80);
        let code = lines[0]
            .iter()
            .find(|s| s.role == Role::Code)
            .expect("code span");
        assert_eq!(code.text, "cargo test");
    }

    #[test]
    fn bold_and_italic_are_styled() {
        let lines = render("**bold** and *italic*", 80);
        assert!(
            lines[0]
                .iter()
                .any(|s| s.role == Role::Strong && s.text == "bold")
        );
        assert!(
            lines[0]
                .iter()
                .any(|s| s.role == Role::Emphasis && s.text == "italic")
        );
    }

    #[test]
    fn snake_case_is_not_italicised() {
        let lines = render("a snake_case identifier", 80);
        assert!(lines[0].iter().all(|s| s.role != Role::Emphasis));
        assert_eq!(joined(&lines), "a snake_case identifier");
    }

    #[test]
    fn a_markdown_link_shows_its_text_as_a_link() {
        let lines = render("see [the docs](https://example.com/x)", 80);
        let link = lines[0]
            .iter()
            .find(|s| s.role == Role::Link)
            .expect("link");
        assert_eq!(link.text, "the docs");
    }

    #[test]
    fn a_bare_url_is_a_link() {
        let lines = render("see https://example.com/x for details", 80);
        let link = lines[0]
            .iter()
            .find(|s| s.role == Role::Link)
            .expect("link");
        assert_eq!(link.text, "https://example.com/x");
    }

    #[test]
    fn a_list_marker_is_styled_and_wraps_hang_under_the_text() {
        let lines = render("- one two three four five", 12);
        assert_eq!(roles(&lines[0])[0], Role::Marker);
        assert!(lines.len() > 1, "{lines:?}");
        assert_eq!(
            &lines[1][0].text[..2],
            "  ",
            "continuation hangs under the text"
        );
    }

    #[test]
    fn no_line_is_wider_than_the_requested_width() {
        let description = "a very long paragraph ".repeat(10) + "\n\n- and a list item too";
        let lines = render(&description, 20);
        for line in &lines {
            let width: usize = line.iter().map(|s| s.text.width()).sum();
            assert!(width <= 20, "{line:?}");
        }
    }

    #[test]
    fn a_blank_line_stays_a_paragraph_break() {
        let lines = render("first\n\nsecond", 80);
        assert_eq!(lines.len(), 3);
        assert!(lines[1].is_empty());
    }

    #[test]
    fn a_table_row_is_verbatim_and_not_word_wrapped() {
        let lines = render("| a | b |", 6);
        assert_eq!(joined(&lines), "| a | \nb |");
    }
}
