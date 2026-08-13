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

        Ok(Pty { master: pair.master, writer, rx, child, dead, cols, rows })
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
    /// The process group currently reading the terminal, and the shell's own.
    ///
    /// This is `tcgetpgrp` on the master side: while a job runs in the
    /// foreground the two differ, and when it ends — however it ends, including
    /// a signal that left it no chance to tidy up — they become equal again.
    /// It is the one account of "a program finished" that does not depend on
    /// the program saying so.
    pub fn foreground_group(&self) -> Option<i32> {
        self.master.process_group_leader().map(|p| p as i32)
    }

    /// The shell we spawned. Its pid is its process group while it sits at a
    /// prompt, which is what `foreground_group` returns then.
    pub fn shell_pid(&self) -> Option<i32> {
        self.child.process_id().map(|p| p as i32)
    }

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

#[cfg(test)]
mod job_tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// The signal the mode reset is built on, against a real shell.
    ///
    /// The job kills *itself* on command rather than being killed from here.
    /// A test that sends a signal to a process group it derived from the code
    /// under test will, the one time that code is wrong, send it to the test
    /// runner's own group instead — which is exactly what happened while this
    /// was being written.
    #[test]
    fn a_job_that_dies_hands_the_terminal_back_to_the_shell() {
        let mut pty = Pty::spawn_with_shell("/bin/sh", 80, 24, || {}).unwrap();
        let shell = pty.shell_pid();
        assert!(shell.is_some(), "no pid for the shell we spawned");

        let settle = |pty: &Pty, at_prompt: bool| {
            let end = Instant::now() + Duration::from_secs(15);
            while Instant::now() < end {
                if (pty.foreground_group() == shell) == at_prompt {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            false
        };

        assert!(settle(&pty, true), "the shell never reached its prompt");

        // A job that waits for a line, then kills itself with no chance to tidy
        // up — the same end a crash comes to.
        pty.write(b"sh -c 'read x; kill -9 $$'\n");
        assert!(settle(&pty, false), "the job never took the terminal");
        assert_ne!(pty.foreground_group(), shell);

        pty.write(b"\n");
        assert!(settle(&pty, true), "the terminal never came back to the shell");
    }
}
