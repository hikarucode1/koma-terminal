//! Pseudo-terminal handling: spawn the user's shell and pump its output onto a
//! channel, waking the winit event loop whenever bytes arrive.

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};

use anyhow::Result;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

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
        let size = PtySize { rows, cols, pixel_width: 0, pixel_height: 0 };
        let pair = native_pty_system().openpty(size)?;

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
        let mut cmd = CommandBuilder::new(&shell);
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

    /// Spawns a shell, runs a command, and returns everything it printed.
    fn run_in_shell(cmd: &str, deadline: Duration) -> String {
        let mut pty = Pty::spawn(80, 24, || {}).expect("could not open a pty");
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
        let out = run_in_shell("echo KOMA-OK\n", Duration::from_secs(20));
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
}
