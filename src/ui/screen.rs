//! Just enough of a terminal to check what a real one would show: cursor moves, printed
//! text, and the OSC 8 link each cell was printed under.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use unicode_width::UnicodeWidthChar;

/// A writer that keeps what was written, since `CrosstermBackend` owns its own.
#[derive(Clone, Default)]
pub struct Sink(pub Arc<Mutex<Vec<u8>>>);

impl Sink {
    /// Everything written since the last call.
    pub fn take(&self) -> String {
        String::from_utf8(std::mem::take(&mut *self.0.lock().unwrap())).unwrap()
    }
}

impl std::io::Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// What each cell holds: its glyph and the link it was printed under.
#[derive(Default)]
pub struct Screen {
    cells: HashMap<(u16, u16), (char, Option<String>)>,
    x: u16,
    y: u16,
    link: Option<String>,
}

impl Screen {
    /// Apply `bytes` as a terminal would. Escapes other than cursor moves and OSC 8 are
    /// skipped.
    pub fn feed(&mut self, bytes: &str) {
        let mut chars = bytes.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '\x1b' {
                let width = c.width().unwrap_or(0) as u16;
                if width > 0 {
                    self.cells.insert((self.x, self.y), (c, self.link.clone()));
                    self.x += width;
                }
                continue;
            }
            match chars.next() {
                Some('[') => {
                    let mut params = String::new();
                    for c in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&c) {
                            if c == 'H' {
                                let mut it = params.split(';').map(|n| n.parse().unwrap_or(1));
                                self.y = it.next().unwrap_or(1) - 1;
                                self.x = it.next().unwrap_or(1) - 1;
                            }
                            break;
                        }
                        params.push(c);
                    }
                }
                Some(']') => {
                    let mut body = String::new();
                    while let Some(c) = chars.next() {
                        if c == '\x07' || (c == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                            break;
                        }
                        body.push(c);
                    }
                    if let Some(rest) = body.strip_prefix("8;") {
                        let url = rest.split_once(';').map_or("", |(_, url)| url);
                        self.link = (!url.is_empty()).then(|| url.to_owned());
                    }
                }
                _ => {}
            }
        }
    }

    /// The link the cell at `(x, y)` was last printed under.
    pub fn link_at(&self, x: u16, y: u16) -> Option<&str> {
        self.cells
            .get(&(x, y))
            .and_then(|(_, link)| link.as_deref())
    }

    /// Whether a link is still open, so the next text printed anywhere would join it.
    pub fn link_open(&self) -> bool {
        self.link.is_some()
    }

    /// Every linked cell on row `y`, by column.
    pub fn linked_in_row(&self, y: u16) -> Vec<(u16, &str)> {
        let mut linked: Vec<(u16, &str)> = self
            .cells
            .iter()
            .filter(|((_, cy), _)| *cy == y)
            .filter_map(|((x, _), (_, link))| link.as_deref().map(|link| (*x, link)))
            .collect();
        linked.sort_unstable();
        linked
    }
}
