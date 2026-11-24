//! Running a command inside a pseudo-terminal, for scenarios such as
//! `sbx exec -it` that refuse to run without one.

use std::io::{Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

/// Outcome of a command run in a pseudo-terminal.
#[derive(Debug)]
pub(crate) struct PtyOutcome {
    /// Exit code; `None` when the command timed out or was killed by a signal.
    pub exit_code: Option<i32>,
    /// Whether the command was killed because it exceeded the timeout.
    pub timed_out: bool,
    /// Everything the command wrote to the terminal.
    pub output: String,
}

/// Runs `program` with `args` in a terminal of `rows` by `cols`, writing
/// `stdin` to it first. With `api`, the command is pointed at that socket
/// through `DOCKER_SANDBOXES_API`. Blocks until the command exits or `timeout`
/// expires, in which case it is killed.
///
/// # Errors
///
/// Fails if the terminal cannot be opened or the command cannot be started.
pub(crate) fn run(
    program: &str,
    args: &[String],
    stdin: Option<&str>,
    api: Option<&Path>,
    (rows, cols): (u16, u16),
    timeout: Duration,
) -> Result<PtyOutcome> {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| anyhow!("opening a pseudo-terminal: {e}"))?;
    let mut command = CommandBuilder::new(program);
    command.args(args);
    if let Ok(cwd) = std::env::current_dir() {
        command.cwd(cwd);
    }
    if let Some(api) = api {
        command.env("DOCKER_SANDBOXES_API", api);
    }
    let mut child = pair
        .slave
        .spawn_command(command)
        .map_err(|e| anyhow!("starting {program} in a pseudo-terminal: {e}"))?;
    // The command holds its own end; keeping ours open would hide the end of
    // the output.
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| anyhow!("reading the pseudo-terminal: {e}"))?;
    let collector = std::thread::spawn(move || {
        let mut output = Vec::new();
        let _ = reader.read_to_end(&mut output);
        output
    });
    if let Some(input) = stdin {
        let mut writer = pair
            .master
            .take_writer()
            .map_err(|e| anyhow!("writing to the pseudo-terminal: {e}"))?;
        writer
            .write_all(input.as_bytes())
            .context("writing the scenario input")?;
    }

    let deadline = Instant::now() + timeout;
    let (exit_code, timed_out) = loop {
        if let Some(status) = child.try_wait().context("waiting for the command")? {
            break (i32::try_from(status.exit_code()).ok(), false);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break (None, true);
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    // Closing the terminal ends the reader once the command is gone.
    drop(pair.master);
    let output = collector.join().unwrap_or_default();
    Ok(PtyOutcome {
        exit_code,
        timed_out,
        output: String::from_utf8_lossy(&output).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_see_a_terminal_of_the_requested_size() {
        let outcome = run(
            "sh",
            &["-c".to_owned(), "stty size".to_owned()],
            None,
            None,
            (40, 120),
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(outcome.exit_code, Some(0));
        assert_eq!(outcome.output.trim(), "40 120");
    }
}
