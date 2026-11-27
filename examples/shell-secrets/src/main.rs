//! Secrets given to a shell sandbox through the socket, and what the proxy of
//! the sandbox does with them, checked from inside with a public echo service.
//!
//! The example pushes two custom secrets with dummy values, one for the echo
//! service and one for another host, then sends requests from the sandbox to
//! the echo service with a placeholder in the `Authorization` header. The
//! service answers with the headers it received: the proxy writes the value of
//! the echo secret in place of its placeholder, and leaves the placeholder of
//! the other one as it is, since the request does not go to its host. The
//! sandbox never holds a value. The secrets stay with the sandbox through a
//! restart, an update without custom secrets leaves them, and an empty list
//! removes them.
//!
//! Then come the credentials of services that `sbx` knows (`github`,
//! `openai`, …), as `sbx secret set <service> --sandbox` gives them: the
//! daemon records where each comes from, and the sandbox keeps the fixed
//! placeholders it was created with (`GH_TOKEN` for GitHub), never a value.
//!
//! Everything goes through the socket: no `sbx secret` command, nothing
//! stored by the CLI, the secrets go with the sandbox. Only the dummy values
//! leave the machine, to `postman-echo.com` over plain HTTP. The requests are
//! sent by the `get` command of
//! [`sandbox-tools`](../../sandbox-tools/src/main.rs), built for the sandbox
//! (it needs the target: `rustup target add <arch>-unknown-linux-musl`).
//!
//! ```sh
//! just example shell-secrets
//! ```

use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use docker_sandbox::exec::ExecSpec;
use docker_sandbox::sandbox::{
    CredentialSource, CredentialsUpdate, CustomSecret, Sandbox, Secret, ServiceCredential,
};
use example_common::{connect, kit, program, sandbox};

/// Public service answering with the headers it received.
const ECHO_HOST: &str = "postman-echo.com";

/// Host of the second secret, which the requests of the example never reach.
const OTHER_HOST: &str = "api.example.com";

/// Dummy value of the secret of the echo service.
const ECHO_VALUE: &str = "example-dummy-value-not-a-secret";

/// Environment variable the echo secret names for its placeholder.
const ECHO_ENV: &str = "EXAMPLE_ECHO_KEY";

/// Dummy value of the secret of the other host.
const OTHER_VALUE: &str = "example-other-dummy-value";

/// Dummy value of the GitHub credential.
const GITHUB_VALUE: &str = "example-github-dummy-value";

/// Dummy value of the `OpenAI` credential.
const OPENAI_VALUE: &str = "example-openai-dummy-value";

/// Source recorded for the `OpenAI` credential: an environment variable, as
/// the CLI declares the credentials it stores.
const OPENAI_SOURCE: &str = "EXAMPLE_OPENAI_SOURCE";

/// File the search of the values must find, to show that it reads files.
const CANARY: &str = "/tmp/example-search-canary";

/// How long the search of the values through the files may take.
const SEARCH_TIMEOUT: Duration = Duration::from_secs(120);

/// What the echo service received in place of a placeholder.
#[derive(Debug, PartialEq, Eq)]
enum Received {
    /// The value of the echo secret.
    EchoValue,
    /// The value of the other secret.
    OtherValue,
    /// The placeholder, unchanged.
    Placeholder,
    /// Anything else, or no header at all.
    Other(Option<String>),
}

#[tokio::main]
async fn main() -> Result<()> {
    let client = connect().await?;
    let package = kit::package(&client, "shell").await?;
    let name = sandbox::unique_name("secrets");
    sandbox::with_sandbox(&client, &package, &name, |sandbox| async move {
        let running = sandbox.ensure_running().await?;
        program::install_tools(&sandbox).await?;

        // The placeholders are what the sandbox sees; the values only travel
        // over the socket, to the daemon.
        let echo = format!("sbx-cs-echo{}", std::process::id());
        let other = format!("sbx-cs-other{}", std::process::id());
        // A value a program logs by mistake shows redacted; reading it takes
        // an explicit `expose`.
        let value = Secret::new(ECHO_VALUE);
        println!(
            "the value of the echo secret, as a log prints it: {value:?} ({} characters)",
            value.expose_secret().len()
        );
        let secrets = CredentialsUpdate::default()
            .with_custom_secret(
                CustomSecret::new([ECHO_HOST], &echo, value.clone()).with_env_name(ECHO_ENV),
            )
            .with_custom_secret(CustomSecret::new([OTHER_HOST], &other, OTHER_VALUE));
        sandbox.push_credentials(&secrets).await?;
        println!("pushed two custom secrets with dummy values, for {ECHO_HOST} and {OTHER_HOST}");

        expect(&sandbox, &echo, Received::EchoValue).await?;
        println!("{ECHO_HOST} received the value of its secret in place of the placeholder");
        expect(&sandbox, &other, Received::Placeholder).await?;
        println!("it received the placeholder of the {OTHER_HOST} secret unchanged");

        let environment = sandbox.exec(&ExecSpec::new(["env"])).await?.check()?.stdout;
        // The search runs as root, so that no file escapes it, and must find a
        // file written for it: an empty answer then means that no file holds
        // a value, not that none was read.
        let canary = format!("example-search-canary-{}", std::process::id());
        sandbox.write_file(CANARY, format!("{canary}\n")).await?;
        let files = sandbox
            .exec(
                &ExecSpec::new([
                    "grep",
                    "-rlsF",
                    "--exclude-dir=proc",
                    "--exclude-dir=sys",
                    "--exclude-dir=dev",
                    "-e",
                    ECHO_VALUE,
                    "-e",
                    OTHER_VALUE,
                    "-e",
                    &canary,
                    "/",
                ])
                .with_user("root")
                .with_timeout(SEARCH_TIMEOUT),
            )
            .await?;
        let found: Vec<&str> = files.stdout.lines().collect();
        ensure!(
            ![ECHO_VALUE, OTHER_VALUE]
                .iter()
                .any(|value| environment.contains(value)),
            "a value is in the environment of the sandbox"
        );
        ensure!(
            found == [CANARY],
            "the search of the files of the sandbox found {found:?}, not only {CANARY}"
        );
        println!("the sandbox holds neither value: not in its environment, not in its files");
        // `env_name` is recorded with the secret, but the daemon puts no such
        // variable in the environment of an execution.
        ensure!(
            !environment
                .lines()
                .any(|line| line.starts_with(&format!("{ECHO_ENV}="))),
            "{ECHO_ENV} is in the environment"
        );
        println!("no {ECHO_ENV} variable either, although the secret names it");

        // The daemon keeps the secrets of a sandbox through a restart.
        drop(running);
        sandbox.stop().await?;
        let running = sandbox.ensure_running().await?;
        expect(&sandbox, &echo, Received::EchoValue).await?;
        println!("after a stop and a start, without a new push: still the value");

        // An update that carries no custom secrets, a service credential for
        // instance, leaves them as they are.
        sandbox
            .push_credentials(&CredentialsUpdate::default())
            .await?;
        expect(&sandbox, &echo, Received::EchoValue).await?;
        println!("after an update without custom secrets: still the value");

        sandbox
            .push_credentials(&CredentialsUpdate::default().without_custom_secrets())
            .await?;
        expect(&sandbox, &echo, Received::Placeholder).await?;
        println!("after an update with an empty list: the placeholder, unchanged");

        services(&sandbox).await?;
        drop(running);
        Ok(())
    })
    .await
}

/// Gives `sandbox` service credentials with dummy values, the way `sbx
/// secret set <service> --sandbox` does and with an explicit source, then
/// takes them back, checking what the daemon records and what the sandbox
/// holds at each step.
async fn services(sandbox: &Sandbox) -> Result<()> {
    let placeholder = environment(sandbox, "GH_TOKEN").await?;
    println!(
        "the sandbox was created with the placeholder GH_TOKEN={}",
        placeholder.as_deref().unwrap_or("(none)")
    );

    // `push_service_secret` is the shortcut of `sbx secret set github
    // --sandbox`: a value, and a source the CLI names after the sandbox
    // (`CredentialsUpdate::with_sandbox_secret`).
    sandbox.push_service_secret("github", GITHUB_VALUE).await?;
    let explicit = CredentialsUpdate::default()
        .with_service_credential("openai", OPENAI_VALUE)
        .with_credential_source("openai", CredentialSource::env(OPENAI_SOURCE));
    sandbox.push_credentials(&explicit).await?;
    let recorded = sandbox.inspect().await?.credential_sources;
    for (service, source) in &recorded {
        println!("the daemon records the {service} credential, from {source}");
    }
    ensure!(
        recorded.keys().map(String::as_str).eq(["github", "openai"]),
        "unexpected credential sources: {recorded:?}"
    );
    let env = sandbox.exec(&ExecSpec::new(["env"])).await?.check()?.stdout;
    ensure!(
        ![GITHUB_VALUE, OPENAI_VALUE]
            .iter()
            .any(|value| env.contains(value)),
        "a service credential is in the environment of the sandbox"
    );
    ensure!(
        environment(sandbox, "GH_TOKEN").await? == placeholder,
        "GH_TOKEN changed"
    );
    println!("the sandbox still holds the same placeholder, and neither value");

    // `remove_service_secret` is the shortcut of `sbx secret rm github
    // --sandbox` (`CredentialsUpdate::without_sandbox_secret`); by hand, an
    // empty value and the source removed.
    sandbox.remove_service_secret("github").await?;
    let mut taken_back = CredentialsUpdate::default().without_credential_source("openai");
    taken_back
        .credential_values
        .push(ServiceCredential::cleared("openai"));
    sandbox.push_credentials(&taken_back).await?;
    let recorded = sandbox.inspect().await?.credential_sources;
    ensure!(recorded.is_empty(), "credentials left: {recorded:?}");
    println!("both taken back: the daemon records no credential any more");
    Ok(())
}

/// The value of the environment variable `name` in an execution of `sandbox`.
async fn environment(sandbox: &Sandbox, name: &str) -> Result<Option<String>> {
    let env = sandbox.exec(&ExecSpec::new(["env"])).await?.check()?.stdout;
    Ok(env
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{name}=")).map(str::to_owned)))
}

/// Sends `Authorization: Bearer <placeholder>` to the echo service from the
/// sandbox, and fails unless it received what `expected` says.
async fn expect(sandbox: &Sandbox, placeholder: &str, expected: Received) -> Result<()> {
    let received = received(sandbox, placeholder).await?;
    if received != expected {
        bail!("the echo service received {received:?}, not {expected:?}");
    }
    Ok(())
}

/// What the echo service received in the `Authorization` header of a request
/// from the sandbox that carries `placeholder`.
async fn received(sandbox: &Sandbox, placeholder: &str) -> Result<Received> {
    let bearer = format!("Bearer {placeholder}");
    let url = format!("http://{ECHO_HOST}/headers");
    let output = sandbox
        .exec(&ExecSpec::new([
            program::TOOLS,
            "get",
            &url,
            "Authorization",
            &bearer,
        ]))
        .await?
        .check()?;
    let echo: serde_json::Value =
        serde_json::from_str(&output.stdout).context("the echo service answers with JSON")?;
    let header = echo["headers"]["authorization"]
        .as_str()
        .and_then(|header| header.strip_prefix("Bearer "));
    Ok(match header {
        Some(ECHO_VALUE) => Received::EchoValue,
        Some(OTHER_VALUE) => Received::OtherValue,
        Some(value) if value == placeholder => Received::Placeholder,
        other => Received::Other(other.map(str::to_owned)),
    })
}
