//! A web server started in a shell sandbox and published on a port of the
//! host, reached from the host, then unpublished.
//!
//! The server is the `serve` command of
//! [`sandbox-tools`](../../sandbox-tools/src/main.rs), a program of this
//! workspace built as a static binary for the sandbox (it needs the target:
//! `rustup target add <arch>-unknown-linux-musl`), copied into it and started
//! with an interactive execution.
//!
//! ```sh
//! just example shell-ports
//! ```

use std::time::Duration;

use anyhow::{Context, Result};
use docker_sandbox::exec::{AttachSpec, ExecSpec};
use docker_sandbox::sandbox::PortBinding;
use example_common::{connect, kit, program, sandbox};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Port of the server inside the sandbox.
const SANDBOX_PORT: u16 = 8000;

/// How long the server may take to listen.
const START_TIMEOUT: Duration = Duration::from_secs(10);

#[tokio::main]
async fn main() -> Result<()> {
    let client = connect().await?;
    let package = kit::package(&client, "shell").await?;
    let name = sandbox::unique_name("ports");
    let host_port = 40_000 + u16::try_from(std::process::id() % 20_000)?;
    sandbox::with_sandbox(&client, &package, &name, |sandbox| async move {
        let _running = sandbox.ensure_running().await?;
        // The page, written into a directory created first: uploads need an
        // existing directory.
        sandbox
            .exec(&ExecSpec::new(["mkdir", "-p", "/tmp/www"]))
            .await?
            .check()?;
        sandbox
            .write_file("/tmp/www/index.html", "<h1>served from a sandbox</h1>\n")
            .await?;

        // The server runs as long as its execution: until the sandbox goes.
        program::install_tools(&sandbox).await?;
        let port = SANDBOX_PORT.to_string();
        let server = AttachSpec::new([program::TOOLS, "serve", port.as_str(), "/tmp/www"]);
        let (_input, mut output, _control) = sandbox.attach(&server).await?.split();
        let mut started = String::new();
        while !started.contains("listening on") {
            let chunk = tokio::time::timeout(START_TIMEOUT, output.next())
                .await
                .context("the server did not start")?
                .context("the server ended before listening")??;
            started.push_str(&String::from_utf8_lossy(&chunk.data));
        }

        let published = sandbox
            .publish(&[PortBinding::tcp(SANDBOX_PORT, host_port)])
            .await?;
        println!(
            "published sandbox port {SANDBOX_PORT} on {}:{host_port}",
            published[0].host_ip.as_deref().unwrap_or("127.0.0.1")
        );

        let page = fetch(host_port).await?;
        println!("{}", page.lines().last().unwrap_or_default());

        sandbox.unpublish(&published).await?;
        println!("unpublished");
        Ok(())
    })
    .await
}

/// `GET /` on the loopback port, retried while the published port is not
/// ready: until then the connection is refused or reset.
async fn fetch(port: u16) -> Result<String> {
    for _ in 0..20 {
        if let Ok(response) = get(port).await
            && response.split_whitespace().nth(1) == Some("200")
        {
            return Ok(response);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    None.context("the server did not answer on the published port")
}

/// One `GET /` on the loopback port.
async fn get(port: u16) -> std::io::Result<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await?;
    stream
        .write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .await?;
    let mut response = String::new();
    stream.read_to_string(&mut response).await?;
    Ok(response)
}
