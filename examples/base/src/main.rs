//! The first steps with the library: connect to the daemon, look at what it
//! holds, create a sandbox, run a command in it, and delete it.
//!
//! ```sh
//! just example base
//! ```

use anyhow::Result;
use docker_sandbox::exec::ExecSpec;
use example_common::{connect, kit, sandbox};

#[tokio::main]
async fn main() -> Result<()> {
    // One client per program; handles taken from it are cheap to clone.
    let client = connect().await?;
    for existing in client.sandboxes().list().await? {
        println!(
            "sandbox {:<30} {:<8} {}",
            existing.name, existing.status, existing.agent
        );
    }
    println!(
        "{} images held by the daemon",
        client.images().list().await?.len()
    );

    // A sandbox of the `shell` kit, created from a package compiled once.
    let package = kit::package(&client, "shell").await?;
    let name = sandbox::unique_name("base");
    sandbox::with_sandbox(&client, &package, &name, |sandbox| async move {
        // The daemon stops a sandbox 30 seconds after its last session: hold
        // one for as long as the sandbox is used.
        let _running = sandbox.ensure_running().await?;
        let info = sandbox.inspect().await?;
        println!("{} is {} (id {})", info.name, info.status, info.id);

        let output = sandbox
            .exec(&ExecSpec::new(["uname", "-a"]))
            .await?
            .check()?;
        print!("{}", output.stdout);
        Ok(())
    })
    .await
}
