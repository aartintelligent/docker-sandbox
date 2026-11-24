//! Plugging the local terminal into an interactive execution.
//!
//! The terminal is switched to raw mode, so that every key — Ctrl-C included —
//! reaches the program in the sandbox, and window size changes are forwarded.
//! Standard input is read on a thread of its own: a blocking read must not
//! keep the program alive once the execution has ended. When it ends — input
//! piped from a file — the relay types Ctrl-D, as a terminal would: closing the
//! input of a terminal session does not end the program.

use std::io::{IsTerminal, Read};
use std::time::Duration;

use anyhow::{Context, Result};
use docker_sandbox::exec::{ExecSession, TerminalSize};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

/// How often the window size is checked.
const RESIZE_POLL: Duration = Duration::from_millis(250);

/// End-of-transmission (Ctrl-D): what a terminal sends at the end of input.
const END_OF_INPUT: &[u8] = &[0x04];

/// How long the exit code is awaited once the output has ended.
const EXIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Size of the local terminal, 24 × 80 when it is not a terminal.
#[must_use]
pub fn size() -> TerminalSize {
    crossterm::terminal::size().map_or_else(
        |_| TerminalSize::default(),
        |(cols, rows)| TerminalSize::new(rows, cols),
    )
}

/// Relays the local terminal to `session` until its output ends, then returns
/// its exit code.
///
/// # Errors
///
/// Fails when the terminal cannot be switched to raw mode, the connection
/// breaks, or the exit code is not available in time.
pub async fn relay(session: ExecSession) -> Result<i32> {
    let raw = RawMode::enable()?;
    let (mut input, mut output, control) = session.split();

    let (keys, mut received) = mpsc::channel::<Vec<u8>>(64);
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buffer = [0_u8; 1024];
        while let Ok(read) = stdin.read(&mut buffer) {
            if read == 0 || keys.blocking_send(buffer[..read].to_vec()).is_err() {
                break;
            }
        }
    });
    let keyboard = tokio::spawn(async move {
        while let Some(bytes) = received.recv().await {
            if input.write_all(&bytes).await.is_err() {
                return;
            }
        }
        // The input stays open: the output ends when the program does.
        let _ = input.write_all(END_OF_INPUT).await;
        std::future::pending::<()>().await;
    });
    let resizer = {
        let control = control.clone();
        tokio::spawn(async move {
            let mut current = size();
            loop {
                tokio::time::sleep(RESIZE_POLL).await;
                let now = size();
                if now != current && control.resize(now).await.is_ok() {
                    current = now;
                }
            }
        })
    };

    let mut stdout = tokio::io::stdout();
    while let Some(chunk) = output.next().await {
        stdout.write_all(&chunk?.data).await?;
        stdout.flush().await?;
    }
    keyboard.abort();
    resizer.abort();
    drop(raw);
    control
        .wait(EXIT_TIMEOUT)
        .await
        .context("waiting for the exit code")
}

/// Raw mode of the local terminal, restored when dropped.
#[derive(Debug)]
struct RawMode {
    /// Whether raw mode was switched on: not when input is not a terminal.
    enabled: bool,
}

impl RawMode {
    /// Switches the terminal to raw mode when standard input is one.
    fn enable() -> Result<Self> {
        let enabled = std::io::stdin().is_terminal();
        if enabled {
            crossterm::terminal::enable_raw_mode().context("switching the terminal to raw mode")?;
        }
        Ok(Self { enabled })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        if self.enabled {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
}
