use std::io::{self, IsTerminal, Write};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Run a blocking operation with honest, indeterminate progress on stderr.
/// Redirected output gets one status line; JSON callers do not use this helper.
pub(super) fn during<T, E>(
    message: &str,
    operation: impl FnOnce() -> Result<T, E>,
) -> Result<T, E> {
    let terminal =
        io::stderr().is_terminal() && std::env::var("TERM").map_or(true, |term| term != "dumb");
    let mut progress = Progress::new(message, terminal);
    let result = operation();
    progress.stop();
    result
}

struct Progress {
    stop: Option<mpsc::Sender<()>>,
    worker: Option<JoinHandle<()>>,
    terminal: bool,
}

impl Progress {
    fn new(message: &str, terminal: bool) -> Self {
        if !terminal {
            eprintln!("{message}...");
            return Self {
                stop: None,
                worker: None,
                terminal: false,
            };
        }

        let (stop, stopped) = mpsc::channel();
        let message = message.to_owned();
        let worker = thread::spawn(move || {
            let started = Instant::now();
            let frames = ['|', '/', '-', '\\'];
            let mut frame = 0;
            loop {
                let elapsed = started.elapsed().as_secs();
                eprint!("\r{} {}  {}s", frames[frame], message, elapsed);
                let _ = io::stderr().flush();
                frame = (frame + 1) % frames.len();
                match stopped.recv_timeout(Duration::from_millis(100)) {
                    Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                    Err(RecvTimeoutError::Timeout) => {}
                }
            }
        });
        Self {
            stop: Some(stop),
            worker: Some(worker),
            terminal: true,
        }
    }

    fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        if self.terminal {
            eprint!("\r\x1b[2K");
            let _ = io::stderr().flush();
            self.terminal = false;
        }
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        self.stop();
    }
}
