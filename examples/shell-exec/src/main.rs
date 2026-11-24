//! Commands in a shell sandbox: outputs and exit codes, users, working
//! directory and environment, per-command deadlines, standard input, an
//! execution followed by its identifier, and the agent of the kit run without
//! a terminal.
//!
//! ```sh
//! just example shell-exec
//! ```

use std::time::Duration;

use anyhow::Result;
use docker_sandbox::Error;
use docker_sandbox::exec::{AgentSessionSpec, AttachSpec, ExecSpec};
use example_common::{connect, kit, sandbox};

#[tokio::main]
async fn main() -> Result<()> {
    let client = connect().await?;
    let package = kit::package(&client, "shell").await?;
    let name = sandbox::unique_name("exec");
    sandbox::with_sandbox(&client, &package, &name, |sandbox| async move {
        let _running = sandbox.ensure_running().await?;

        // A program and its arguments, not interpreted by a shell.
        let whoami = sandbox.exec(&ExecSpec::new(["id", "-un"])).await?.check()?;
        println!("default user: {}", whoami.stdout.trim());

        // A shell script, as root, in a working directory, with a variable.
        let script = ExecSpec::shell("echo \"$GREETING from $(pwd) as $(id -un)\"")
            .with_user("0")
            .with_working_dir("/tmp")
            .with_env("GREETING", "hello");
        print!("{}", sandbox.exec(&script).await?.check()?.stdout);

        // A non-zero exit code is an output; `check` turns it into an error
        // that keeps the outputs.
        let failing = sandbox
            .exec(&ExecSpec::shell("echo failing >&2; exit 3"))
            .await?;
        println!("exit code {}", failing.exit_code);
        if let Err(Error::CommandFailed(output)) = failing.check() {
            println!(
                "checked: code {}, stderr {:?}",
                output.exit_code,
                output.stderr.trim()
            );
        }

        // A missing program is not an error either: it exits with 127.
        let missing = sandbox.exec(&ExecSpec::new(["no-such-program"])).await?;
        println!("missing program: exit code {}", missing.exit_code);

        // A command can have its own deadline, shorter than the client's.
        let slow = ExecSpec::new(["sleep", "10"]).with_timeout(Duration::from_secs(1));
        match sandbox.exec(&slow).await {
            Err(error) if error.is_timeout() => println!("deadline: {error}"),
            other => println!("unexpected: {other:?}"),
        }

        // An interactive execution reads standard input.
        let counted = sandbox
            .attach(&AttachSpec::new(["wc", "-l"]))
            .await?
            .collect("one\ntwo\nthree\n", Duration::from_secs(10))
            .await?;
        println!(
            "wc -l read {} lines",
            String::from_utf8_lossy(&counted.stdout).trim()
        );

        // An execution followed by its identifier, as a program that hands it
        // to another task, or polls it, does.
        let session = sandbox
            .attach(&AttachSpec::shell("sleep 1; exit 3"))
            .await?;
        let id = session.id().to_owned();
        let (_input, _output, control) = session.split();
        let state = sandbox.exec_inspect(&id).await?;
        let short = &id[..12];
        println!("execution {short}: running {}", state.running);
        let code = control.wait(Duration::from_secs(10)).await?;
        println!("execution {short}: ended with exit code {code}");
        // Once the daemon has reported its end, it forgets the execution.
        match sandbox.exec_inspect(&id).await {
            Err(error) if error.is_not_found() => {
                println!("execution {short}: forgotten by the daemon once ended");
            }
            Err(error) => return Err(error.into()),
            Ok(state) => println!("execution {short}: still known, running {}", state.running),
        }

        // The agent of the kit, bash for `shell`, in a session without a
        // terminal: its standard output and error come apart, as an
        // unattended program needs them.
        let spec = AgentSessionSpec::default()
            .without_tty()
            .with_args(["-c", "echo out; echo err >&2"]);
        let agent = sandbox
            .agent_session(&spec)
            .await?
            .collect("", Duration::from_secs(10))
            .await?;
        println!(
            "agent session without a terminal: stdout {:?}, stderr {:?}",
            String::from_utf8_lossy(&agent.stdout),
            String::from_utf8_lossy(&agent.stderr)
        );
        Ok(())
    })
    .await
}
