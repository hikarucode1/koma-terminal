//! Pseudo-terminal handling: spawn the user's shell and pump its output onto a
//! channel, waking the winit event loop whenever bytes arrive.

use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::io::RawFd;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};

use anyhow::Result;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

/// Shells to try, in order, when `$SHELL` isn't usable. zsh leads because it is
/// the macOS default; the last is guaranteed by POSIX.
const SHELL_CANDIDATES: [&str; 3] = ["/bin/zsh", "/bin/bash", "/bin/sh"];

/// Picks the shell to run, given `$SHELL` and a way to ask whether a path
/// exists. Taking `exists` as a parameter keeps the ordering testable without
/// depending on which shells happen to be installed.
///
/// The previous code fell back to `/bin/zsh` unconditionally, which meant koma
/// refused to start at all on a Linux box with no zsh and no `$SHELL` — a
/// container or a bare service session, for instance.
fn resolve_shell_in(env_shell: Option<String>, exists: impl Fn(&str) -> bool) -> String {
    if let Some(s) = env_shell {
        let s = s.trim();
        // A stale `$SHELL` pointing at an uninstalled shell would reproduce the
        // very failure this function exists to prevent, so absolute paths are
        // checked. A bare name like `zsh` is resolved through PATH by the spawn
        // itself and can't be checked here, so it is passed along as given.
        if !s.is_empty() && (!s.starts_with('/') || exists(s)) {
            return s.to_string();
        }
    }
    SHELL_CANDIDATES
        .iter()
        .find(|p| exists(p))
        .map(|p| p.to_string())
        // Nothing found: hand the OS the POSIX path and let spawn report why.
        .unwrap_or_else(|| "/bin/sh".to_string())
}

fn resolve_shell(env_shell: Option<String>) -> String {
    resolve_shell_in(env_shell, |p| Path::new(p).exists())
}

fn default_shell() -> String {
    resolve_shell(std::env::var("SHELL").ok())
}

pub struct Pty {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    rx: Receiver<FromPty>,
    child: Box<dyn Child + Send + Sync>,
    /// Flipped by the reader thread when the shell's output stream ends.
    dead: Arc<AtomicBool>,
    cols: u16,
    rows: u16,
    /// The shell's pid, which is also the id of its process group: it is
    /// spawned into a new session on the slave, so it leads its own group and
    /// every job it starts gets a group of its own.
    shell_pgid: Option<i32>,
}

/// What the reader thread hands to the UI thread, in the order it happened.
///
/// The handover is a point *in* the stream, not a fact about the stream, which
/// is why it travels alongside the bytes instead of being asked about
/// afterwards: by the time the UI thread gets around to looking, the kernel
/// only knows who holds the terminal *now*.
pub enum FromPty {
    /// Output, to be parsed.
    Bytes(Vec<u8>),
    /// The shell took the terminal back here, and whoever had it is gone.
    Handover,
}

/// The edge detector behind "a program has stopped owning the terminal".
///
/// Lives in the reader thread, where the readings are taken. Keeping the state
/// in one place with the liveness test injected is what lets the whole thing be
/// exercised without arranging real process groups.
struct ForegroundWatch {
    shell_pgid: Option<i32>,
    /// Whether the shell held the terminal at the last look. Starts true —
    /// nothing has run yet — so the first reading cannot report a handover
    /// that never happened.
    shell_in_front: bool,
    /// The group that held it before the shell got it back, kept so the
    /// handover can be asked the follow-up question: gone, or only stopped?
    last_program_pgid: Option<i32>,
}

impl ForegroundWatch {
    fn new(shell_pgid: Option<i32>) -> Self {
        ForegroundWatch { shell_pgid, shell_in_front: true, last_program_pgid: None }
    }

    /// Feeds in one reading of who holds the terminal. True once each time the
    /// shell takes it back, which is the kernel's own answer to "the
    /// foreground program has finished".
    ///
    /// Worth going to the kernel for because it does not depend on the program
    /// behaving. A program that is killed cleans nothing up and says nothing
    /// on the wire, but it still loses the terminal, and `tcgetpgrp` still
    /// says so. Callers use it to undo what the program left set — see
    /// `Grid::soft_reset`.
    ///
    /// **Suspending is not finishing.** `Ctrl-Z` hands the terminal back just
    /// as dying does, but the program is still there, holding everything it
    /// set and expecting to find it on `fg`. So the handover asks one more
    /// question — does the group that had the terminal still exist? — and
    /// stays quiet when it does. A program stopped and then killed while
    /// stopped is therefore never cleaned up: the shell already has the
    /// terminal, so no second handover is coming.
    ///
    /// Three ways this stays silent. All leave the old behaviour in place
    /// rather than introducing a new failure, but none is narrow:
    ///
    /// - **No job control** (`set +m`, or a non-interactive shell): jobs share
    ///   the shell's own group, so there is no handover to see.
    /// - **A nested shell in front.** The comparison is against the *top*
    ///   shell's pgid — the one koma spawned — so anything that puts another
    ///   shell between it and the program hides the handover completely:
    ///   `ssh host`, `su`, `docker exec -it`, or a plain `bash`. Kill a
    ///   mouse-enabled program inside any of those and the terminal goes back
    ///   to the *inner* shell's group, which never equals `shell_pgid`. Over
    ///   ssh — the case koma is most used through — issue #19's symptom is
    ///   therefore untouched.
    /// - **A program that lives and dies inside one read.** Readings are taken
    ///   once per read, so a program whose whole life fits between two of them
    ///   is never seen in front, and the shell was never away. Its bytes are
    ///   still parsed, so whatever it set leaks. Sampling is as fine as the
    ///   stream is chunked and no finer.
    fn sample(&mut self, front: Option<i32>, still_alive: impl Fn(i32) -> bool) -> bool {
        let Some(shell) = self.shell_pgid else { return false };
        // No answer is not the same as "someone else has it": leave the last
        // reading in place rather than inventing a handover on the way back.
        let Some(front) = front else { return false };
        let now_shell = front == shell;
        let returned = now_shell && !self.shell_in_front;
        self.shell_in_front = now_shell;
        if !now_shell {
            self.last_program_pgid = Some(front);
            return false;
        }
        if !returned {
            return false;
        }
        // Taken either way: on `fg` the group takes the terminal again and the
        // branch above records it afresh.
        match self.last_program_pgid.take() {
            Some(pgid) => !still_alive(pgid),
            // Never saw who had it — the handover is all we know, so report it.
            None => true,
        }
    }
}

/// Who the kernel gives the terminal to, read straight from a fd we own.
#[cfg(unix)]
fn foreground_pgid_of(fd: RawFd) -> Option<i32> {
    let pgid = unsafe { libc::tcgetpgrp(fd) };
    if pgid < 0 { None } else { Some(pgid) }
}

/// Whether a process group still exists.
///
/// Signal 0 delivers nothing but runs every existence and permission check the
/// kernel would run for a real signal, which is exactly the question. `EPERM`
/// is a yes: the group is there, it is simply not ours to signal.
#[cfg(unix)]
fn process_group_alive(pgid: i32) -> bool {
    if unsafe { libc::kill(-pgid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn process_group_alive(_pgid: i32) -> bool {
    false
}

impl Pty {
    /// `wake` is called from the reader thread each time a chunk arrives, so the
    /// UI thread can be nudged out of its wait.
    pub fn spawn<F>(cols: u16, rows: u16, wake: F) -> Result<Self>
    where
        F: Fn() + Send + 'static,
    {
        Self::spawn_with_shell(&default_shell(), cols, rows, wake)
    }

    /// As `spawn`, with the shell chosen explicitly rather than from `$SHELL`.
    /// Lets tests drive a specific shell regardless of the environment they
    /// happen to run under.
    pub fn spawn_with_shell<F>(shell: &str, cols: u16, rows: u16, wake: F) -> Result<Self>
    where
        F: Fn() + Send + 'static,
    {
        let size = PtySize { rows, cols, pixel_width: 0, pixel_height: 0 };
        let pair = native_pty_system().openpty(size)?;

        let mut cmd = CommandBuilder::new(shell);
        // Login shell, so the user's normal profile applies.
        cmd.arg("-l");
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        if let Ok(cwd) = std::env::current_dir() {
            cmd.cwd(cwd);
        }

        let child = pair.slave.spawn_command(cmd)?;
        // The slave fd must be closed here or we'd never see EOF on exit.
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;

        let (tx, rx): (Sender<FromPty>, Receiver<FromPty>) = channel();
        let dead = Arc::new(AtomicBool::new(false));
        let dead_w = dead.clone();

        let shell_pgid = child.process_id().map(|pid| pid as i32);
        // A copy of our own, so the reader thread never races the master's
        // lifetime — an fd number closed on one thread can be handed straight
        // back out to something else on another.
        #[cfg(unix)]
        let watch_fd = pair.master.as_raw_fd().and_then(|fd| {
            let dup = unsafe { libc::dup(fd) };
            (dup >= 0).then_some(dup)
        });
        #[cfg(not(unix))]
        let watch_fd: Option<i32> = None;

        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            let mut watch = ForegroundWatch::new(shell_pgid);
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        // Asked here, once per read, because this is the only
                        // place that knows *when* these bytes turned up. The
                        // UI thread can be a repaint or a resize behind, and
                        // by then the kernel has forgotten there was ever a
                        // program in front.
                        //
                        // Before the bytes, not after: output arriving now is
                        // the shell's — its "Killed" line and a fresh prompt —
                        // and the prompt is where a shell re-arms the modes it
                        // owns. Clearing first lets those land on top.
                        #[cfg(unix)]
                        let front = watch_fd.and_then(foreground_pgid_of);
                        #[cfg(not(unix))]
                        let front = None;
                        if watch.sample(front, process_group_alive)
                            && tx.send(FromPty::Handover).is_err()
                        {
                            break;
                        }
                        if tx.send(FromPty::Bytes(buf[..n].to_vec())).is_err() {
                            break;
                        }
                        wake();
                    }
                }
            }
            #[cfg(unix)]
            if let Some(fd) = watch_fd {
                unsafe { libc::close(fd) };
            }
            dead_w.store(true, Ordering::Release);
            wake();
        });

        Ok(Pty { master: pair.master, writer, rx, child, dead, cols, rows, shell_pgid })
    }

    /// The process group the kernel currently gives the terminal to. `None`
    /// where the platform has no such notion, or when the pty is gone.
    #[cfg(unix)]
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn foreground_pgid(&self) -> Option<i32> {
        self.master.process_group_leader()
    }

    #[cfg(not(unix))]
    pub fn foreground_pgid(&self) -> Option<i32> {
        None
    }

    /// The shell's own process group id, for comparing against `foreground_pgid`.
    /// The production path compares the two inside
    /// `foreground_returned_to_shell`; this is for tests that want to see them
    /// separately.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn shell_pgid(&self) -> Option<i32> {
        self.shell_pgid
    }

    /// Drains everything buffered without blocking, handovers included and in
    /// the order they happened.
    pub fn read_events(&mut self) -> Vec<FromPty> {
        let mut out = Vec::new();
        while let Ok(event) = self.rx.try_recv() {
            out.push(event);
        }
        out
    }

    /// Everything buffered, with the handovers dropped. For tests and for
    /// callers that only want the output.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn read_available(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        for event in self.read_events() {
            if let FromPty::Bytes(b) = event {
                out.extend_from_slice(&b);
            }
        }
        out
    }

    pub fn write(&mut self, bytes: &[u8]) {
        let _ = self.writer.write_all(bytes);
        let _ = self.writer.flush();
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        let (cols, rows) = (cols.max(1), rows.max(1));
        if cols == self.cols && rows == self.rows {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        let _ = self.master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
    }

    /// True once the shell has exited *and* its output has been fully consumed.
    pub fn is_dead(&mut self) -> bool {
        if self.dead.load(Ordering::Acquire) {
            return true;
        }
        matches!(self.child.try_wait(), Ok(Some(_)))
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Runs a command in `shell` and returns everything it printed.
    fn run_in(shell: &str, cmd: &str, deadline: Duration) -> String {
        let mut pty = Pty::spawn_with_shell(shell, 80, 24, || {})
            .unwrap_or_else(|e| panic!("could not start {shell:?}: {e}"));
        pty.write(cmd.as_bytes());

        let mut out = String::new();
        let start = Instant::now();
        while start.elapsed() < deadline {
            let chunk = pty.read_available();
            if chunk.is_empty() {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            out.push_str(&String::from_utf8_lossy(&chunk));
            if out.contains("KOMA-OK") {
                break;
            }
        }
        pty.kill();
        out
    }

    #[test]
    fn shell_output_comes_back_through_the_pty() {
        let out = run_in(&default_shell(), "echo KOMA-OK\n", Duration::from_secs(20));
        assert!(out.contains("KOMA-OK"), "shell produced: {out:?}");
    }

    #[test]
    fn terminal_size_is_visible_to_the_shell() {
        // `stty size` reads the pty's window size, so this proves the ioctl took.
        let mut pty = Pty::spawn(100, 30, || {}).expect("could not open a pty");
        pty.write(b"stty size\n");
        let mut out = String::new();
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(20) && !out.contains("30 100") {
            let chunk = pty.read_available();
            if chunk.is_empty() {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            out.push_str(&String::from_utf8_lossy(&chunk));
        }
        pty.kill();
        assert!(out.contains("30 100"), "stty reported: {out:?}");
    }

    /// Polls `f` until it holds, or gives up. Real shells take their time
    /// starting, so every wait here is generous rather than tuned.
    fn wait_until(pty: &mut Pty, deadline: Duration, mut f: impl FnMut(&mut Pty) -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < deadline {
            let _ = pty.read_available();
            if f(pty) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn the_shell_leads_its_own_process_group() {
        // The premise the handover check rests on: the shell is spawned into a
        // new session on the slave, so it leads a group whose id is its pid,
        // and it holds the terminal whenever nothing else is running.
        let mut pty = Pty::spawn(80, 24, || {}).expect("could not open a pty");
        let shell = pty.shell_pgid().expect("the shell should have a pid");
        let settled =
            wait_until(&mut pty, Duration::from_secs(20), |p| p.foreground_pgid() == Some(shell));
        let front = pty.foreground_pgid();
        pty.kill();
        assert!(settled, "the shell should hold the terminal at its prompt, but {front:?} did");
    }

    #[test]
    fn the_watch_reports_a_program_going_away() {
        let mut w = ForegroundWatch::new(Some(100));
        let gone = |_: i32| false;
        assert!(!w.sample(Some(100), gone), "the shell was in front all along");
        assert!(!w.sample(Some(200), gone), "a program taking the terminal is not a handover");
        assert!(w.sample(Some(100), gone), "and giving it back is");
        assert!(!w.sample(Some(100), gone), "reported once, not on every reading after");
    }

    #[test]
    fn the_watch_stays_quiet_for_a_program_that_is_only_stopped() {
        let mut w = ForegroundWatch::new(Some(100));
        let alive = |_: i32| true;
        w.sample(Some(200), alive);
        assert!(!w.sample(Some(100), alive), "stopped is not gone");

        // Resumed with `fg`, then killed for real.
        w.sample(Some(200), alive);
        assert!(w.sample(Some(100), |_: i32| false), "the second time it really had gone");
    }

    #[test]
    fn a_reading_that_failed_neither_invents_a_handover_nor_loses_one() {
        let mut w = ForegroundWatch::new(Some(100));
        let gone = |_: i32| false;
        w.sample(Some(200), gone);
        assert!(!w.sample(None, gone), "no answer is not the same as the shell being back");
        assert!(w.sample(Some(100), gone), "the real handover still lands");
    }

    /// Everything the pane would have parsed, up to the first handover and
    /// after it. `None` for the first half when no handover was reported.
    fn split_at_handover(events: Vec<FromPty>) -> (Option<Vec<u8>>, Vec<u8>) {
        let mut before = Vec::new();
        let mut after = Vec::new();
        let mut seen = false;
        for e in events {
            match e {
                FromPty::Handover => seen = true,
                FromPty::Bytes(b) if seen => after.extend_from_slice(&b),
                FromPty::Bytes(b) => before.extend_from_slice(&b),
            }
        }
        (seen.then_some(before), after)
    }

    /// Drains for `how_long`, keeping everything in order.
    fn collect_for(pty: &mut Pty, how_long: Duration) -> Vec<FromPty> {
        let start = Instant::now();
        let mut out = Vec::new();
        while start.elapsed() < how_long {
            out.extend(pty.read_events());
            std::thread::sleep(Duration::from_millis(20));
        }
        out.extend(pty.read_events());
        out
    }

    #[test]
    fn a_killed_foreground_job_still_hands_the_terminal_back() {
        // The case behind issue #19. The job dies by SIGKILL, so it sends no
        // escape sequence to say it is done — nothing on the wire marks the
        // end. The kernel moves the terminal back regardless, which is the
        // whole reason this is asked of the kernel and not of the byte stream.
        let mut pty = Pty::spawn(80, 24, || {}).expect("could not open a pty");
        let shell = pty.shell_pgid().expect("the shell should have a pid");
        assert!(
            wait_until(&mut pty, Duration::from_secs(20), |p| p.foreground_pgid() == Some(shell)),
            "the shell never reached a prompt"
        );

        // It has to say *something* first. Readings are taken where the bytes
        // are read, so a job that never writes is never seen in front and its
        // death is never seen as a return — which costs nothing, because a
        // program that wrote nothing set nothing either. Issue #19's program
        // took the mouse, so this one does too.
        pty.write(b"sh -c 'printf \"\\033[?1002h\"; sleep 1; kill -9 $$'\n");
        let events = collect_for(&mut pty, Duration::from_secs(6));
        pty.kill();
        assert!(split_at_handover(events).0.is_some(), "the handover was never reported");
    }

    #[test]
    fn a_suspended_program_has_not_finished() {
        // Ctrl-Z hands the terminal back exactly as dying does, but vim is
        // still sitting there holding its alternate screen and its mouse, and
        // expects to find them on `fg`. Cleaning up here would hand it back a
        // terminal it no longer recognises.
        let mut pty = Pty::spawn(80, 24, || {}).expect("could not open a pty");
        let shell = pty.shell_pgid().expect("the shell should have a pid");
        assert!(
            wait_until(&mut pty, Duration::from_secs(20), |p| p.foreground_pgid() == Some(shell)),
            "the shell never reached a prompt"
        );

        pty.write(b"sleep 30\n");
        assert!(
            wait_until(&mut pty, Duration::from_secs(20), |p| {
                p.foreground_pgid().is_some_and(|f| f != shell)
            }),
            "the job never took the terminal"
        );
        let _ = pty.read_events(); // Anything from starting it is not the point.

        pty.write(b"\x1a"); // Ctrl-Z: the line discipline sends SIGTSTP.
        let events = collect_for(&mut pty, Duration::from_secs(4));
        let back = pty.foreground_pgid() == Some(shell);
        pty.write(b"kill -9 %1\n");
        pty.kill();

        assert!(back, "the shell should hold the terminal while the job is stopped");
        assert!(split_at_handover(events).0.is_none(), "a stopped job is not a finished one");
    }

    #[test]
    fn a_program_that_lives_and_dies_between_two_pumps_is_still_noticed() {
        // The reading has to be taken where the bytes are read. Ask the kernel
        // later — from a UI thread that was busy with a resize — and it only
        // knows who holds the terminal *now*, which is the shell, the same
        // answer it gave before the program ever started.
        let mut pty = Pty::spawn(80, 24, || {}).expect("could not open a pty");
        let shell = pty.shell_pgid().expect("the shell should have a pid");
        assert!(
            wait_until(&mut pty, Duration::from_secs(20), |p| p.foreground_pgid() == Some(shell)),
            "the shell never reached a prompt"
        );
        let _ = pty.read_events();

        // Takes the mouse, then is killed without a word — all of it inside
        // the stall below, so the UI thread never looks while it is in front.
        pty.write(b"sh -c 'printf \"\\033[?1002h\"; sleep 0.3; kill -9 $$'\n");
        std::thread::sleep(Duration::from_secs(3));

        let (before, after) = split_at_handover(pty.read_events());
        pty.kill();
        let before = before.expect("the handover was lost to the stall");
        assert!(
            before.windows(8).any(|w| w == b"\x1b[?1002h"[..8].as_ref()),
            "the program's own bytes belong before the handover, not after"
        );
        assert!(!after.is_empty(), "the shell's prompt belongs after it");
    }

    #[test]
    fn exiting_the_shell_marks_the_pty_dead() {
        let mut pty = Pty::spawn(80, 24, || {}).expect("could not open a pty");
        pty.write(b"exit\n");
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(20) && !pty.is_dead() {
            let _ = pty.read_available();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(pty.is_dead(), "the pty should report EOF once the shell exits");
    }

    #[test]
    fn the_waker_fires_when_output_arrives() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let mut pty = Pty::spawn(80, 24, move || {
            h.fetch_add(1, Ordering::Release);
        })
        .expect("could not open a pty");
        pty.write(b"echo KOMA-OK\n");
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(20) && hits.load(Ordering::Acquire) == 0 {
            std::thread::sleep(Duration::from_millis(20));
        }
        pty.kill();
        assert!(hits.load(Ordering::Acquire) > 0, "the wake callback never fired");
    }

    #[test]
    fn an_explicit_shell_wins() {
        assert_eq!(resolve_shell_in(Some("/usr/bin/fish".into()), |_| true), "/usr/bin/fish");
    }

    #[test]
    fn a_bare_shell_name_is_left_for_path_lookup() {
        // `zsh` is resolved through PATH by the spawn, so we can't check it
        // here — passing it through unchanged is the only correct behaviour.
        assert_eq!(resolve_shell_in(Some("zsh".into()), |_| false), "zsh");
    }

    #[test]
    fn an_absolute_shell_that_is_gone_falls_back() {
        // A stale $SHELL left behind by an uninstalled shell would otherwise
        // reproduce the exact ENOENT this change exists to prevent.
        let picked = resolve_shell_in(Some("/usr/bin/fish".into()), |p| p == "/bin/bash");
        assert_eq!(picked, "/bin/bash");
    }

    #[test]
    fn surrounding_whitespace_is_trimmed_off() {
        // The guard used to trim before testing but return the untrimmed value,
        // so " /bin/bash " passed the check and then failed to spawn.
        let picked = resolve_shell_in(Some(" /bin/bash ".into()), |p| p == "/bin/bash");
        assert_eq!(picked, "/bin/bash");
    }

    #[test]
    fn a_blank_shell_is_treated_as_unset() {
        for blank in ["", "   "] {
            assert_eq!(
                resolve_shell_in(Some(blank.into()), |_| true),
                resolve_shell_in(None, |_| true),
                "{blank:?} should behave exactly like no $SHELL at all"
            );
        }
    }

    #[test]
    fn the_fallback_walks_the_candidates_in_order() {
        // Each case removes the previous winner, so the whole order is pinned
        // by behaviour rather than by restating the constant.
        assert_eq!(resolve_shell_in(None, |_| true), "/bin/zsh");
        assert_eq!(resolve_shell_in(None, |p| p != "/bin/zsh"), "/bin/bash");
        assert_eq!(resolve_shell_in(None, |p| p == "/bin/sh"), "/bin/sh");
    }

    #[test]
    fn a_system_with_no_known_shell_still_names_one() {
        // Nothing sensible is left, so hand the OS the POSIX path and let the
        // spawn produce a real error rather than panicking here.
        assert_eq!(resolve_shell_in(None, |_| false), "/bin/sh");
    }

    #[test]
    fn the_real_filesystem_yields_a_shell_that_exists() {
        let shell = resolve_shell(None);
        assert!(Path::new(&shell).exists(), "picked {shell:?}, which does not exist");
    }

    #[test]
    fn a_pty_starts_on_the_fallback_shell() {
        // The regression itself. Going through spawn_with_shell means this
        // exercises the fallback even on a machine where $SHELL is set — which
        // is every developer machine, and was why the first version of this
        // test passed without touching the code path it claimed to cover.
        let shell = resolve_shell(None);
        let out = run_in(&shell, "echo KOMA-OK\n", Duration::from_secs(20));
        assert!(out.contains("KOMA-OK"), "{shell:?} produced: {out:?}");
    }
}
