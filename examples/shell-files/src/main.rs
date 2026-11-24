//! Files in and out of a shell sandbox: a configuration file, an executable
//! script, whole directories in both directions, and tar archives as they
//! are, for a program that stores or streams them itself.
//!
//! ```sh
//! just example shell-files
//! ```

use anyhow::{Context, Result};
use docker_sandbox::exec::ExecSpec;
use example_common::{connect, kit, sandbox};

#[tokio::main]
async fn main() -> Result<()> {
    let client = connect().await?;
    let package = kit::package(&client, "shell").await?;
    let name = sandbox::unique_name("files");

    // A small project on the host, copied into the sandbox below.
    let host = std::env::temp_dir().join(format!("example-files-{}", std::process::id()));
    let project = host.join("project");
    std::fs::create_dir_all(project.join("src")).context("creating the host project")?;
    std::fs::write(project.join("src/data.txt"), "1\n2\n3\n")?;

    let outcome = sandbox::with_sandbox(&client, &package, &name, |sandbox| {
        let (host, project) = (host.clone(), project.clone());
        async move {
            let _running = sandbox.ensure_running().await?;

            // A single file, then an executable script.
            sandbox
                .write_file("/tmp/settings.toml", "level = \"debug\"\n")
                .await?;
            let settings = sandbox.read_file("/tmp/settings.toml").await?;
            print!("settings.toml: {}", String::from_utf8_lossy(&settings));
            sandbox
                .write_file_with_mode(
                    "/tmp/sum.sh",
                    "#!/bin/sh\nawk '{ s += $1 } END { print s }' \"$1\" > /tmp/project/result.txt\n",
                    0o755,
                )
                .await?;

            // The host directory lands under its own name: /tmp/project.
            sandbox.upload_dir(&project, "/tmp").await?;
            sandbox
                .exec(&ExecSpec::new(["/tmp/sum.sh", "/tmp/project/src/data.txt"]))
                .await?
                .check()?;

            // And comes back, results included, under ./back/project.
            sandbox
                .download_dir("/tmp/project", host.join("back"))
                .await?;
            let result = std::fs::read_to_string(host.join("back/project/result.txt"))?;
            println!("sum computed in the sandbox: {}", result.trim());

            // The same directory as a tar archive, kept as bytes, then
            // extracted elsewhere: the directory must exist first.
            let archive = sandbox.download_archive("/tmp/project").await?;
            sandbox
                .exec(&ExecSpec::new(["mkdir", "-p", "/tmp/copy"]))
                .await?
                .check()?;
            sandbox.upload_archive("/tmp/copy", archive.clone()).await?;
            let copied = sandbox
                .exec(&ExecSpec::new(["cat", "/tmp/copy/project/result.txt"]))
                .await?
                .check()?;
            println!(
                "archive of {} bytes extracted again: result.txt reads {}",
                archive.len(),
                copied.stdout.trim()
            );
            Ok(())
        }
    })
    .await;
    let _ = std::fs::remove_dir_all(&host);
    outcome
}
