//! `Ctrl-C` while a statement is running, measured.
//!
//! The shell only watches for a press where there is a terminal, so
//! this test gives it one: a pseudoterminal, the pair the operating
//! system hands out for exactly this, with the shell on the far side of
//! it reading keys and drawing a progress line the way it would under a
//! person. Driving it over a pipe instead would test the other branch,
//! the one that runs the statement inline and cannot be interrupted at
//! all.
//!
//! The press itself is a `SIGINT` sent to the child rather than a `^C`
//! byte written into the terminal, because the byte only becomes a
//! signal when the driver's signal characters are on and the child is
//! the foreground process group of a controlling terminal, which is
//! session bookkeeping this test would have to do to arrive at the
//! same signal. What the terminal driver does on `Ctrl-C` is send this
//! signal, so this is the press with the driver's part played out.
//!
//! Unix only. Windows has no pseudoterminal with this shape and no
//! `SIGINT` to send one, and the console equivalent is a different
//! mechanism the shell does not use yet.
#![cfg(unix)]

use std::ffi::{CStr, c_char, c_int};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::FromRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// What the milestone asks for, from the press to the shell saying it
/// stopped.
const BUDGET: Duration = Duration::from_millis(50);

/// Enough people that every pair of them is minutes of work, so the
/// statement is certainly still running when the press arrives and the
/// test never races the answer.
const PEOPLE: u64 = 12_000;

/// `SIGINT`, which is 2 everywhere a unix runs.
const SIGINT: c_int = 2;

const O_RDWR: c_int = 2;

/// Do not take this terminal as a controlling one. The parent is a test
/// binary that already has its own, and a slave opened without this
/// flag by a process that is a session leader would take it over.
#[cfg(target_os = "linux")]
const O_NOCTTY: i32 = 0o400;
#[cfg(not(target_os = "linux"))]
const O_NOCTTY: i32 = 0x0002_0000;

unsafe extern "C" {
    fn posix_openpt(flags: c_int) -> c_int;
    fn grantpt(fd: c_int) -> c_int;
    fn unlockpt(fd: c_int) -> c_int;
    fn ptsname(fd: c_int) -> *mut c_char;
    fn kill(pid: c_int, sig: c_int) -> c_int;
}

/// The two ends of a fresh pseudoterminal: the master this test writes
/// keys into and reads the screen out of, and the path of the slave the
/// child opens as its own terminal.
fn pty() -> (File, String) {
    // Safety: the four calls are the sequence `posix_openpt` documents,
    // each checked, and the name is copied out before anything else can
    // call `ptsname` again on this thread.
    unsafe {
        let master = posix_openpt(O_RDWR | O_NOCTTY);
        assert!(master >= 0, "no pseudoterminal");
        assert_eq!(grantpt(master), 0, "grantpt");
        assert_eq!(unlockpt(master), 0, "unlockpt");
        let name = ptsname(master);
        assert!(!name.is_null(), "ptsname");
        let name = CStr::from_ptr(name).to_string_lossy().into_owned();
        (File::from_raw_fd(master), name)
    }
}

/// The screen, as the shell paints it, with the moment each piece of it
/// arrived.
#[derive(Default)]
struct Screen {
    chunks: Vec<(Instant, String)>,
}

impl Screen {
    /// The moment the whole of `wanted` had arrived, or `None` while it
    /// has not. The time is the end of the chunk that completed it,
    /// which is when a person would have seen it.
    fn when(&self, wanted: &str) -> Option<Instant> {
        let mut seen = String::new();
        for (at, chunk) in &self.chunks {
            seen.push_str(chunk);
            if seen.contains(wanted) {
                return Some(*at);
            }
        }
        None
    }

    fn text(&self) -> String {
        self.chunks.iter().map(|(_, c)| c.as_str()).collect()
    }
}

/// Reads the master until the child hangs up, timestamping what it
/// reads. A thread of its own because a read on a terminal blocks and
/// the test has to be watching the clock while the shell is quiet.
fn watch(mut master: File) -> Arc<Mutex<Screen>> {
    let screen = Arc::new(Mutex::new(Screen::default()));
    let out = Arc::clone(&screen);
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = master.read(&mut buf) {
            if n == 0 {
                break;
            }
            let at = Instant::now();
            let text = String::from_utf8_lossy(&buf[..n]).into_owned();
            out.lock().expect("screen").chunks.push((at, text));
        }
    });
    screen
}

/// Waits for `wanted` to appear on the screen and answers when it did,
/// or gives up after `patience` and says what was there instead.
fn until(screen: &Mutex<Screen>, wanted: &str, patience: Duration) -> Instant {
    let start = Instant::now();
    while start.elapsed() < patience {
        if let Some(at) = screen.lock().expect("screen").when(wanted) {
            return at;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let seen = screen.lock().expect("screen").text();
    panic!("waited {patience:?} for {wanted:?}, saw {seen:?}");
}

/// A database with enough people in it, written the fast way rather
/// than a statement at a time.
fn seeded(path: &std::path::Path) {
    let mut db = zu::zu1::file::Zu1File::create(path).expect("create");
    zu::zu1::graph::bulk_load_as(&mut db, "person", "knows", PEOPLE, &[(0, 1)]).expect("load");
}

/// Types a line into the shell the way a keyboard does, return key and
/// all. Raw mode is on while the editor is reading, so the key is a
/// carriage return and not a newline.
fn typed(keys: &mut File, line: &str) {
    keys.write_all(line.as_bytes()).expect("type");
    keys.write_all(b"\r").expect("return");
    keys.flush().expect("flush");
}

/// Kills the shell however far it got, so a failed assertion does not
/// leave a process holding a terminal open.
struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn a_press_stops_a_long_statement_inside_the_budget() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("press.zu1");
    seeded(&path);

    let (master, slave) = pty();
    let terminal = || {
        OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(O_NOCTTY)
            .open(&slave)
            .expect("slave")
    };
    let child = Command::new(env!("CARGO_BIN_EXE_zu"))
        .arg("shell")
        .arg(&path)
        .stdin(Stdio::from(terminal()))
        .stdout(Stdio::from(terminal()))
        .stderr(Stdio::from(terminal()))
        .spawn()
        .expect("spawn");
    let pid = child.id() as c_int;
    let child = Reaped(child);
    // Keys go into the master, which is the end a keyboard is on: what
    // is written to the slave comes back out of the master as output,
    // which would be this test reading its own typing.
    let mut keys = master.try_clone().expect("keys");
    let screen = watch(master);

    // The prompt, so the press cannot land on a shell that is still
    // opening the file.
    until(&screen, "zu>", Duration::from_secs(30));
    typed(
        &mut keys,
        "MATCH (a:person), (b:person) WHERE a.id < b.id RETURN count(a) AS n",
    );
    // The progress line, which the shell only writes once a statement
    // has been running for a while: waiting for it is how this test
    // knows the press lands on a statement that is inside the executor
    // rather than one still being parsed.
    until(&screen, "running", Duration::from_secs(30));

    let pressed = Instant::now();
    // Safety: a signal to a child this test spawned and has not reaped.
    assert_eq!(unsafe { kill(pid, SIGINT) }, 0, "kill");
    let stopped = until(&screen, "interrupted at", Duration::from_secs(30));
    let took = stopped.duration_since(pressed);
    assert!(took < BUDGET, "the press took {took:?} to arrive");

    // And the session is the one it was: the shell says so, and then
    // answers the next statement without being reopened.
    let seen = screen.lock().expect("screen").text();
    assert!(seen.contains("the session is still open"), "got {seen:?}");
    typed(&mut keys, "MATCH (p:person) RETURN count(p) AS n");
    until(&screen, &PEOPLE.to_string(), Duration::from_secs(30));
    drop(child);
}
