//! Pseudo-terminal handling: spawn the user's shell and pump its output onto a
//! channel, waking the winit event loop whenever bytes arrive.

use std::io::{Read, Write};
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
    rx: Receiver<Vec<u8>>,
    child: Box<dyn Child + Send + Sync>,
    /// Flipped by the reader thread when the shell's output stream ends.
    dead: Arc<AtomicBool>,
    cols: u16,
    rows: u16,
    /// The shell's pid, which is also the id of its process group: it is
    /// spawned into a new session on the slave, so it leads its own group and
    /// every job it starts gets a group of its own.
    shell_pgid: Option<i32>,
    /// Whether the shell held the terminal at the last look. Starts true —
    /// nothing has run yet — so the first check cannot report a handover that
    /// never happened.
    shell_in_front: bool,
    /// The group that held the terminal before the shell got it back, kept so
    /// the handover can be asked the follow-up question: is it gone, or was it
    /// only stopped?
    last_program_pgid: Option<i32>,
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

        let (tx, rx): (Sender<Vec<u8>>, Receiver<Vec<u8>>) = channel();
        let dead = Arc::new(AtomicBool::new(false));
        let dead_w = dead.clone();

        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                        wake();
                    }
                }
            }
            dead_w.store(true, Ordering::Release);
            wake();
        });

        let shell_pgid = child.process_id().map(|pid| pid as i32);

        Ok(Pty {
            master: pair.master,
            writer,
            rx,
            child,
            dead,
            cols,
            rows,
            shell_pgid,
            shell_in_front: true,
            last_program_pgid: None,
        })
    }

    /// The process group the kernel currently gives the terminal to. `None`
    /// where the platform has no such notion, or when the pty is gone.
    #[cfg(unix)]
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

    /// True once each time the shell takes the terminal back, which is the
    /// kernel's own answer to "the foreground program has finished".
    ///
    /// Worth going to the kernel for because it does not depend on the program
    /// behaving. A program that is killed cleans nothing up and says nothing on
    /// the wire, but it still loses the terminal, and `tcgetpgrp` still says
    /// so. Callers use it to undo what the program left set — see
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
    /// Two ways this stays silent. Both leave the old behaviour in place
    /// rather than introducing a new failure, but neither is narrow:
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
    pub fn foreground_returned_to_shell(&mut self) -> bool {
        let Some(shell) = self.shell_pgid else { return false };
        // No answer is not the same as "someone else has it": leave the last
        // reading in place rather than inventing a handover on the way back.
        let Some(front) = self.foreground_pgid() else { return false };
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
            Some(pgid) => !process_group_alive(pgid),
            // Never saw who had it — the handover is all we know, so report it.
            None => true,
        }
    }

    /// Drains everything buffered without blocking.
    pub fn read_available(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        while let Ok(chunk) = self.rx.try_recv() {
            out.extend_from_slice(&chunk);
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

        pty.write(b"sh -c 'sleep 1; kill -9 $$'\n");
        let took_over = wait_until(&mut pty, Duration::from_secs(20), |p| {
            p.foreground_pgid().is_some_and(|f| f != shell)
        });
        assert!(took_over, "job control should give the job a group of its own");
        // Consumed by the polling above, which is exactly how `pump` uses it.
        assert!(!pty.foreground_returned_to_shell(), "the job is still running");

        let handed_back =
            wait_until(&mut pty, Duration::from_secs(20), |p| p.foreground_returned_to_shell());
        pty.kill();
        assert!(handed_back, "the shell should get the terminal back from a killed job");
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
                    // Consumed here, which is also what records who had it.
                    && !p.foreground_returned_to_shell()
            }),
            "the job never took the terminal"
        );

        pty.write(b"\x1a"); // Ctrl-Z: the line discipline sends SIGTSTP.
        let back =
            wait_until(&mut pty, Duration::from_secs(20), |p| p.foreground_pgid() == Some(shell));
        let reported = pty.foreground_returned_to_shell();
        pty.write(b"kill -9 %1\n");
        let _ = wait_until(&mut pty, Duration::from_secs(5), |_| false);
        pty.kill();

        assert!(back, "the shell should hold the terminal while the job is stopped");
        assert!(!reported, "a stopped job is not a finished one");
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
