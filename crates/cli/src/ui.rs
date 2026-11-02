//! The terminal while a progress bar is shown: the bar stays on the last line, and messages
//! (`note!`) clear it first, so they are not printed into it; it is drawn again on the
//! next update.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// Whether a progress bar occupies the current line of standard error.
pub static BAR: AtomicBool = AtomicBool::new(false);
/// Held while writing to the terminal, so a message and a bar update do not interleave.
pub static TERM: Mutex<()> = Mutex::new(());

/// Clear the progress bar's line (call with TERM held).
pub fn clear_bar() {
    if BAR.load(Ordering::Relaxed) {
        eprint!("\r\x1b[K");
    }
}

/// eprintln!, clearing a progress bar's line first.
#[macro_export]
macro_rules! note {
    ($($t:tt)*) => {{
        let _term = $crate::ui::TERM.lock().unwrap_or_else(|e| e.into_inner());
        $crate::ui::clear_bar();
        eprintln!($($t)*);
    }};
}

/// Columns of the terminal on standard error (80 when unknown).
pub fn width() -> usize {
    // SAFETY: TIOCGWINSZ fills a winsize.
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(2, libc::TIOCGWINSZ, &mut ws) } == 0;
    if ok && ws.ws_col > 0 { ws.ws_col as usize } else { 80 }
}
