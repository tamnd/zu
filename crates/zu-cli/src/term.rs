//! The terminal, in the three calls an interactive shell needs: is a
//! person there, how wide is the window, and stop the driver from
//! interpreting what they type.
//!
//! This is the only platform-specific code in the CLI, and it is here
//! rather than in a dependency because a line editor is the one part of
//! `zu shell` a user judges in the first ten seconds and the cost of
//! owning it is one file. What it does not do is as important: no
//! terminfo, no capability database, no alternate screen. Every escape
//! this program writes is ANSI, which is what every terminal on the
//! seven tier 1 platforms speaks, Windows included since the console
//! learned virtual terminal sequences in Windows 10 1803, and that is
//! the floor `install.ps1` already claims.
//!
//! Raw mode is entered around editing and left around everything else,
//! rather than held for the life of the process. A statement that runs
//! for a minute therefore runs with the driver back to normal, which is
//! what makes `Ctrl-C` at that moment the signal a user expects rather
//! than a byte nobody is reading. It also means a panic in the query
//! path cannot leave the terminal in raw mode, since the guard is
//! dropped on the way out.

use std::io::Write;

/// What a terminal was set to before this program touched it, and the
/// promise to put it back.
///
/// Restoring in `Drop` rather than at the end of the loop is what
/// covers the paths nobody writes: an error return, a panic, a `?` in a
/// function added later.
pub(crate) struct Raw(imp::Saved);

impl Raw {
    /// Puts the terminal in raw mode, or reports why it could not.
    ///
    /// The error is the operating system's, unchanged, because the two
    /// reasons this fails are not being a terminal and not having
    /// permission to change one, and both are clearer in its words than
    /// in a sentence of ours.
    pub(crate) fn enter() -> std::io::Result<Self> {
        imp::enter().map(Raw)
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        imp::restore(&self.0);
    }
}

/// Whether a person is at the other end of standard input.
///
/// This is what decides between the shell a user edits in and the JSONL
/// loop a harness drives, so it asks about the input rather than about
/// the output: `zu shell graph.zu1 | tee log` is still someone typing.
pub(crate) fn interactive() -> bool {
    imp::is_tty()
}

/// How many columns the window has, or 80 where the answer is unknown.
///
/// Asked per redraw rather than cached, because a window that is
/// resized while the cursor sits in an editor is the common case and
/// listening for the signal that says so is a second mechanism for the
/// same fact.
pub(crate) fn width() -> usize {
    imp::width().unwrap_or(80)
}

/// Writes bytes to the terminal and flushes them.
///
/// Every escape this program emits goes through here, so that a redraw
/// is one write and one flush: two writes is a flicker a user sees on a
/// slow link.
pub(crate) fn emit(bytes: &str) -> std::io::Result<()> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    out.write_all(bytes.as_bytes())?;
    out.flush()
}

#[cfg(unix)]
mod imp {
    use std::ffi::c_int;

    // The flags of `c_lflag` this clears, which are the four that stand
    // between a keystroke and the program: line buffering, echo, the
    // signal characters, and the driver's own editing.
    //
    // Two flags of `c_iflag` go with them. `ICRNL` is on by default and
    // turns the return key's carriage return into a newline, which
    // would arrive here as `Ctrl-J` and add a line to a statement
    // instead of running it: the two keys are one key to a program that
    // leaves it on. `IXON` is the driver's flow control, which eats
    // `Ctrl-S` and `Ctrl-Q` and freezes a terminal that a user is
    // typing into, which is the oldest way to make an editor look
    // hung. Nothing else in `c_iflag` or `c_oflag` is touched, so a
    // newline this program writes still gets the carriage return the
    // driver adds.
    #[cfg(target_os = "linux")]
    const ICRNL: Flag = 0x100;
    #[cfg(target_os = "linux")]
    const IXON: Flag = 0x400;
    #[cfg(not(target_os = "linux"))]
    const ICRNL: Flag = 0x100;
    #[cfg(not(target_os = "linux"))]
    const IXON: Flag = 0x200;

    #[cfg(target_os = "linux")]
    const ICANON: Flag = 0x2;
    #[cfg(target_os = "linux")]
    const ECHO: Flag = 0x8;
    #[cfg(target_os = "linux")]
    const ISIG: Flag = 0x1;
    #[cfg(target_os = "linux")]
    const IEXTEN: Flag = 0x8000;
    #[cfg(target_os = "linux")]
    const VTIME: usize = 5;
    #[cfg(target_os = "linux")]
    const VMIN: usize = 6;
    #[cfg(target_os = "linux")]
    const TIOCGWINSZ: c_ulong = 0x5413;

    #[cfg(not(target_os = "linux"))]
    const ICANON: Flag = 0x100;
    #[cfg(not(target_os = "linux"))]
    const ECHO: Flag = 0x8;
    #[cfg(not(target_os = "linux"))]
    const ISIG: Flag = 0x80;
    #[cfg(not(target_os = "linux"))]
    const IEXTEN: Flag = 0x400;
    #[cfg(not(target_os = "linux"))]
    const VTIME: usize = 17;
    #[cfg(not(target_os = "linux"))]
    const VMIN: usize = 16;
    #[cfg(not(target_os = "linux"))]
    const TIOCGWINSZ: c_ulong = 0x4008_7468;

    const TCSANOW: c_int = 0;
    const STDIN: c_int = 0;

    #[cfg(target_os = "linux")]
    type Flag = u32;
    #[cfg(not(target_os = "linux"))]
    type Flag = u64;
    #[allow(non_camel_case_types)]
    type c_ulong = u64;

    /// The layout `tcgetattr` fills in, which is two layouts: glibc and
    /// musl agree on it, and the BSD one the Macs use has no line
    /// discipline byte, twenty control characters instead of thirty
    /// two, and a word-sized flag. Getting this wrong is a stack
    /// smash rather than a wrong answer, which is why the two are
    /// spelled separately rather than approximated by one.
    #[cfg(target_os = "linux")]
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub(super) struct Saved {
        iflag: Flag,
        oflag: Flag,
        cflag: Flag,
        lflag: Flag,
        line: u8,
        cc: [u8; 32],
        ispeed: Flag,
        ospeed: Flag,
    }

    #[cfg(not(target_os = "linux"))]
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub(super) struct Saved {
        iflag: Flag,
        oflag: Flag,
        cflag: Flag,
        lflag: Flag,
        cc: [u8; 20],
        ispeed: Flag,
        ospeed: Flag,
    }

    #[repr(C)]
    struct WinSize {
        rows: u16,
        cols: u16,
        xpixels: u16,
        ypixels: u16,
    }

    unsafe extern "C" {
        fn isatty(fd: c_int) -> c_int;
        fn tcgetattr(fd: c_int, termios: *mut Saved) -> c_int;
        fn tcsetattr(fd: c_int, actions: c_int, termios: *const Saved) -> c_int;
        fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    }

    pub(super) fn is_tty() -> bool {
        // SAFETY: a question about a file descriptor, with no memory
        // in the call at all.
        unsafe { isatty(STDIN) == 1 }
    }

    pub(super) fn enter() -> std::io::Result<Saved> {
        // SAFETY: `tcgetattr` fills the struct it is handed, which is
        // the layout above for this platform, and reports rather than
        // writes when the descriptor is not a terminal.
        let saved = unsafe {
            let mut saved = std::mem::zeroed::<Saved>();
            if tcgetattr(STDIN, &raw mut saved) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            saved
        };
        let mut raw = saved;
        raw.lflag &= !(ICANON | ECHO | ISIG | IEXTEN);
        raw.iflag &= !(ICRNL | IXON);
        // A read returns as soon as there is one byte, and never on a
        // timer. Anything else turns a keystroke into a delay a user
        // reads as the program being slow.
        raw.cc[VMIN] = 1;
        raw.cc[VTIME] = 0;
        // SAFETY: the same struct, handed back to the call that filled
        // it.
        if unsafe { tcsetattr(STDIN, TCSANOW, &raw const raw) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(saved)
    }

    pub(super) fn restore(saved: &Saved) {
        // SAFETY: as above. The result is dropped because this runs in
        // `Drop`, where there is nobody left to tell.
        unsafe {
            tcsetattr(STDIN, TCSANOW, saved);
        }
    }

    pub(super) fn width() -> Option<usize> {
        let mut size = WinSize {
            rows: 0,
            cols: 0,
            xpixels: 0,
            ypixels: 0,
        };
        // SAFETY: the request writes a `winsize` and this passes one.
        let ok = unsafe { ioctl(STDIN, TIOCGWINSZ, &raw mut size) } == 0;
        // A zero column count is what a terminal that does not know its
        // own size reports, and dividing by it later would be worse
        // than not asking.
        (ok && size.cols > 0).then_some(size.cols as usize)
    }
}

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;

    const STD_INPUT: u32 = -10i32 as u32;
    const STD_OUTPUT: u32 = -11i32 as u32;

    // What the console does for a program that reads lines, which is
    // everything this editor does itself.
    const ENABLE_PROCESSED_INPUT: u32 = 0x0001;
    const ENABLE_LINE_INPUT: u32 = 0x0002;
    const ENABLE_ECHO_INPUT: u32 = 0x0004;
    // The two that make a Windows console an ANSI terminal in both
    // directions: escape sequences out, and escape sequences in for the
    // arrow keys, so one decoder serves every platform.
    const ENABLE_VIRTUAL_TERMINAL_INPUT: u32 = 0x0200;
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Coord {
        x: i16,
        y: i16,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Rect {
        left: i16,
        top: i16,
        right: i16,
        bottom: i16,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct ScreenInfo {
        size: Coord,
        cursor: Coord,
        attributes: u16,
        window: Rect,
        max: Coord,
    }

    /// Both console modes as they were, because raw mode here is a
    /// change to the input handle and to the output handle and putting
    /// one back is putting back half of it.
    #[derive(Clone, Copy)]
    pub(super) struct Saved {
        input: u32,
        output: u32,
    }

    unsafe extern "system" {
        fn GetStdHandle(which: u32) -> *mut c_void;
        fn GetConsoleMode(handle: *mut c_void, mode: *mut u32) -> i32;
        fn SetConsoleMode(handle: *mut c_void, mode: u32) -> i32;
        fn GetConsoleScreenBufferInfo(handle: *mut c_void, info: *mut ScreenInfo) -> i32;
    }

    /// The mode of a standard handle, which is `None` when that handle
    /// is a pipe or a file: only a console has one, so this is the
    /// question "is this a terminal" spelled the way Windows spells it.
    fn mode(which: u32) -> Option<(*mut c_void, u32)> {
        let mut mode = 0;
        // SAFETY: both calls take a handle this process owns and write
        // through a pointer to a `u32` on this stack.
        unsafe {
            let handle = GetStdHandle(which);
            (GetConsoleMode(handle, &raw mut mode) != 0).then_some((handle, mode))
        }
    }

    pub(super) fn is_tty() -> bool {
        mode(STD_INPUT).is_some()
    }

    pub(super) fn enter() -> std::io::Result<Saved> {
        let Some((input, saved_input)) = mode(STD_INPUT) else {
            return Err(std::io::Error::last_os_error());
        };
        let Some((output, saved_output)) = mode(STD_OUTPUT) else {
            return Err(std::io::Error::last_os_error());
        };
        let raw_input = (saved_input
            & !(ENABLE_PROCESSED_INPUT | ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT))
            | ENABLE_VIRTUAL_TERMINAL_INPUT;
        // SAFETY: a mode written back to the handle it was read from.
        unsafe {
            if SetConsoleMode(input, raw_input) == 0 {
                return Err(std::io::Error::last_os_error());
            }
            SetConsoleMode(output, saved_output | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
        }
        Ok(Saved {
            input: saved_input,
            output: saved_output,
        })
    }

    pub(super) fn restore(saved: &Saved) {
        // SAFETY: as above, and the results are dropped for the same
        // reason: this runs in `Drop`.
        unsafe {
            if let Some((input, _)) = mode(STD_INPUT) {
                SetConsoleMode(input, saved.input);
            }
            if let Some((output, _)) = mode(STD_OUTPUT) {
                SetConsoleMode(output, saved.output);
            }
        }
    }

    pub(super) fn width() -> Option<usize> {
        let (handle, _) = mode(STD_OUTPUT)?;
        // SAFETY: the call fills the struct it is handed.
        let info = unsafe {
            let mut info = std::mem::zeroed::<ScreenInfo>();
            (GetConsoleScreenBufferInfo(handle, &raw mut info) != 0).then_some(info)?
        };
        // The window rather than the buffer, because a console's buffer
        // is usually taller and sometimes wider than what is on screen,
        // and wrapping happens at the window.
        let cols = info.window.right - info.window.left + 1;
        (cols > 0).then_some(cols as usize)
    }
}

/// Everywhere else, which today is the tier 2 platforms of
/// `platforms.toml`.
///
/// Reporting no terminal rather than guessing at one is what makes the
/// shell fall back to the line-at-a-time loop there, which works
/// without a line editor and is what a pipe gets anyway. A tier 2
/// platform gets a working `zu shell`, not an editing one, and that is
/// a smaller promise honestly kept.
#[cfg(not(any(unix, windows)))]
mod imp {
    pub(super) struct Saved;

    pub(super) fn is_tty() -> bool {
        false
    }

    pub(super) fn enter() -> std::io::Result<Saved> {
        Err(std::io::Error::other(
            "no terminal support on this platform",
        ))
    }

    pub(super) fn restore(_saved: &Saved) {}

    pub(super) fn width() -> Option<usize> {
        None
    }
}
