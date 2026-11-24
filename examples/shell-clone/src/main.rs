//! A Git repository given to a shell sandbox in clone mode, worked on inside,
//! and the work fetched back on the host, through the socket alone.
//!
//! In clone mode (`CreateRequest::with_cloned_workspace`, `sbx run --clone`)
//! the sandbox works on a private clone of the repository, at the same path
//! and with its history, while the repository of the host is mounted
//! read-only at `/run/sandbox/source`: the agent can neither change it nor
//! push to it, and its commits stay in the sandbox until the host fetches
//! them.
//!
//! The `sbx` CLI fetches them through a Git remote whose `uploadpack` runs
//! `sbx upload-pack <sandbox>`: `git upload-pack` started in the sandbox, its
//! standard input and output relayed over the socket. This example does the
//! same with the library: `git fetch --upload-pack` runs the example binary
//! again as `upload-pack <sandbox> <repository>`, which relays the exchange of
//! Git to an interactive execution of `git upload-pack` in the sandbox. No
//! remote is written to the configuration of the repository.
//!
//! The repository is made for the run under `examples/target/clone/`, and
//! left there to be looked at (`git log sandbox/main`).
//!
//! ```sh
//! just example shell-clone
//! ```

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use docker_sandbox::Client;
use docker_sandbox::exec::{AttachSpec, ExecSpec, OutputStream};
use example_common::{connect, kit, sandbox};
use tokio::io::AsyncWriteExt;

/// Directory of the repositories made by the runs.
const RUNS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../target/clone");

/// First argument with which `git fetch` runs the example binary as the
/// relay of `git upload-pack`.
const RELAY: &str = "upload-pack";

/// User of the sandbox that owns the clone, as the CLI runs `git upload-pack`.
const AGENT_UID: &str = "1000";

/// How long `git upload-pack` may take to exit once Git is done.
const EXIT_TIMEOUT: Duration = Duration::from_secs(30);

/// What the agent does in the clone: look at the history, commit a file,
/// then try to write to the repository of the host.
const AGENT_WORK: &str = "\
pwd
git log --format=%s
printf 'written in the sandbox\\n' > AGENT.md
git add AGENT.md
git -c user.name=agent -c user.email=agent@example.invalid -c commit.gpgsign=false \
    commit -q -m 'agent work'
touch /run/sandbox/source/AGENT.md 2>/dev/null && echo source-writable || echo source-read-only";

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let [command, sandbox, repository, ..] = args.as_slice()
        && command == RELAY
    {
        let code = upload_pack(sandbox, repository).await?;
        std::process::exit(code);
    }

    let client = connect().await?;
    let package = kit::package(&client, "shell").await?;
    let name = sandbox::unique_name("clone");
    let repository = seed(&PathBuf::from(RUNS).join(&name))?;
    println!("repository of the host: one commit, `seed`");

    let request = package.request(&name)?.with_cloned_workspace(&repository);
    sandbox::with_sandbox_from(&client, &package, &request, |sandbox| async move {
        let _running = sandbox.ensure_running().await?;
        let output = sandbox
            .exec(&ExecSpec::shell(AGENT_WORK))
            .await?
            .check()?;
        let lines: Vec<&str> = output.stdout.lines().collect();
        ensure!(
            lines == [repository.as_str(), "seed", "source-read-only"],
            "unexpected work in the sandbox: {lines:?}"
        );
        println!("in the sandbox: a clone at the same path, with the history, and the source read-only");
        println!("the agent committed `agent work` in its clone");
        ensure!(
            git(Path::new(&repository), &["log", "--format=%s"])? == "seed\n",
            "the commit reached the repository of the host on its own"
        );
        println!("the repository of the host still has one commit");

        // `git fetch` runs the relay through a shell, with the URL appended:
        // the URL only names the sandbox, as the remote of the CLI does.
        let exe = std::env::current_exe().context("finding the example binary")?;
        let upload_pack = format!(
            "'{}' {RELAY} {} '{repository}'",
            exe.display(),
            sandbox.name()
        );
        git(
            Path::new(&repository),
            &[
                "fetch",
                "-q",
                "--upload-pack",
                &upload_pack,
                &format!("/sandbox/{}", sandbox.name()),
                "+refs/heads/*:refs/remotes/sandbox/*",
            ],
        )?;
        let fetched = git(Path::new(&repository), &["log", "--format=%s", "sandbox/main"])?;
        ensure!(
            fetched == "agent work\nseed\n",
            "unexpected history fetched: {fetched:?}"
        );
        let file = git(Path::new(&repository), &["show", "sandbox/main:AGENT.md"])?;
        println!(
            "fetched on the host as sandbox/main: `agent work` on top of `seed`, AGENT.md reads {:?}",
            file.trim_end()
        );
        Ok(())
    })
    .await
}

/// Relays standard input and output to `git upload-pack repository` in the
/// sandbox `name`, and returns its exit code: the `uploadpack` side of a
/// `git fetch` from the sandbox.
async fn upload_pack(name: &str, repository: &str) -> Result<i32> {
    let client = Client::connect().await?;
    let sandbox = client.sandbox(name);
    let mut spec = AttachSpec::new(["git", "upload-pack", repository])
        .with_user(AGENT_UID)
        .with_working_dir("/");
    // Git asks for its protocol version through the environment.
    if let Ok(protocol) = std::env::var("GIT_PROTOCOL") {
        spec = spec.with_env("GIT_PROTOCOL", protocol);
    }
    let (mut input, mut output, control) = sandbox.attach(&spec).await?.split();
    tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        if tokio::io::copy(&mut stdin, &mut input).await.is_ok() {
            let _ = input.shutdown().await;
        }
    });
    let mut stdout = tokio::io::stdout();
    let mut stderr = tokio::io::stderr();
    while let Some(chunk) = output.next().await {
        let chunk = chunk?;
        // Git waits for each answer before it sends more: flush every chunk.
        match chunk.stream {
            OutputStream::Stdout => {
                stdout.write_all(&chunk.data).await?;
                stdout.flush().await?;
            }
            OutputStream::Stderr => stderr.write_all(&chunk.data).await?,
        }
    }
    Ok(control.wait(EXIT_TIMEOUT).await?)
}

/// Makes a repository with one commit in `dir`, and returns its canonical
/// path: the clone of the sandbox sits at that same path.
fn seed(dir: &Path) -> Result<String> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    git(dir, &["init", "-q", "-b", "main", "repo"])?;
    let repository = dir.join("repo").canonicalize()?;
    std::fs::write(
        repository.join("README.md"),
        "made by the shell-clone example\n",
    )?;
    git(&repository, &["add", "README.md"])?;
    git(&repository, &["commit", "-q", "-m", "seed"])?;
    repository
        .to_str()
        .map(str::to_owned)
        .context("the path of the repository is not UTF-8")
}

/// Runs `git args` in `dir`, without the hooks, signing and identity of the
/// user, and returns its standard output.
fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = std::process::Command::new("git")
        .current_dir(dir)
        .args([
            "-c",
            "user.name=example",
            "-c",
            "user.email=example@example.invalid",
        ])
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .output()
        .context("running git")?;
    if !output.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8(output.stdout)?)
}
