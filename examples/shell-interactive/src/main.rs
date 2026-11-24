//! An interactive shell in a sandbox, relayed to the local terminal: keys go
//! to the shell, its output comes back, and resizing the window resizes its
//! terminal. Leave it with `exit`.
//!
//! ```sh
//! just example shell-interactive
//! ```

use anyhow::Result;
use docker_sandbox::exec::AttachSpec;
use example_common::{connect, kit, sandbox, terminal};

#[tokio::main]
async fn main() -> Result<()> {
    let client = connect().await?;
    let package = kit::package(&client, "shell").await?;
    let name = sandbox::unique_name("interactive");
    sandbox::with_sandbox(&client, &package, &name, |sandbox| async move {
        let _running = sandbox.ensure_running().await?;
        let spec = AttachSpec::new(["bash", "-l"]).with_tty(terminal::size());
        let code = terminal::relay(sandbox.attach(&spec).await?).await?;
        eprintln!("shell exited with code {code}");
        Ok(())
    })
    .await
}
