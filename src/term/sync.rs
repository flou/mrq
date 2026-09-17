//! The DEC 2026 synchronized-output marker pair that brackets one frame.

use std::io;

/// Run `draw` between `BeginSynchronizedUpdate` and `EndSynchronizedUpdate`.
///
/// A terminal otherwise repaints as the frame's bytes arrive, so a redraw that spans
/// thousands of cells can show half-painted for a moment. The markers make the emulator
/// hold its display until the frame is complete. A terminal without DEC 2026 ignores
/// the pair — an unknown private-mode set/reset — so this needs no capability gate.
///
/// The end marker is written even when the draw fails, since leaving sync mode on would
/// hold every later frame off the screen.
pub fn frame<T, F>(draw: F) -> io::Result<T>
where
    F: FnOnce() -> T,
{
    use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};

    let mut out = io::stdout();
    crossterm::execute!(out, BeginSynchronizedUpdate)?;
    let result = draw();
    let _ = crossterm::execute!(out, EndSynchronizedUpdate);
    Ok(result)
}
