//! `live-probe`: the program the live tests of `docker-sandbox` run inside a
//! sandbox.
//!
//! The live tests check what happens inside a sandbox — a published port, the
//! secrets the proxy of the sandbox puts in requests — so something has to
//! listen or send from there. They build this binary statically, upload it
//! with `Sandbox::write_file_with_mode` and start it with an execution:
//!
//! - `serve-once <port> <line>` listens on `<port>` of every interface, prints
//!   `listening on <port>` once it does, and answers the first connection
//!   with `<line>`;
//! - `http <url> [<header> <value>]` sends `GET <url>`, plain HTTP, through the
//!   proxy of the sandbox (`HTTP_PROXY`, unless `NO_PROXY` names the host), and
//!   prints the body of a successful answer.
//!
//! Failures exit with code 1 and a message on standard error.

#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "a command-line tool reports on standard output and standard error"
)]

use std::process::ExitCode;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::header::{HOST, USER_AGENT};
use hyper::{Request, Uri};
use hyper_util::rt::TokioIo;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};

/// What a command returns.
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Usage, printed for unknown arguments.
const USAGE: &str = "usage: live-probe serve-once <port> <line> | http <url> [<header> <value>]";

/// User agent of the requests, so that services refusing anonymous clients
/// answer.
const AGENT: &str = "docker-sandbox-live-probe";

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let outcome = match args.as_slice() {
        ["serve-once", port, line] => serve_once(port, line).await,
        ["http", url] => http(url, None).await,
        ["http", url, name, value] => http(url, Some((name, value))).await,
        _ => Err(USAGE.into()),
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("live-probe: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Answers the first connection on `port` with `line`.
async fn serve_once(port: &str, line: &str) -> Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", port.parse::<u16>()?)).await?;
    println!("listening on {port}");
    let (mut stream, _) = listener.accept().await?;
    stream.write_all(format!("{line}\n").as_bytes()).await?;
    stream.shutdown().await?;
    Ok(())
}

/// Sends `GET url` with the extra `header`, and prints the body of a
/// successful answer.
async fn http(url: &str, header: Option<(&str, &str)>) -> Result<()> {
    let uri: Uri = url.parse()?;
    if uri.scheme_str() != Some("http") {
        return Err(format!("{url}: only http:// URLs are supported").into());
    }
    let host = uri.host().ok_or("the URL has no host")?;
    let port = uri.port_u16().unwrap_or(80);
    // Through a proxy, the request names the whole URL; directly, its path.
    let (address, target) = match proxy_for(host) {
        Some(proxy) => (proxy, uri.to_string()),
        None => (
            (host.to_owned(), port),
            uri.path_and_query()
                .map_or_else(|| "/".to_owned(), ToString::to_string),
        ),
    };
    let stream = TcpStream::connect((address.0.as_str(), address.1)).await?;
    let (mut sender, connection) =
        hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
    tokio::spawn(connection);

    let authority = uri.authority().map_or(host, |a| a.as_str());
    let mut request = Request::get(target)
        .header(HOST, authority)
        .header(USER_AGENT, AGENT);
    if let Some((name, value)) = header {
        request = request.header(name, value);
    }
    let response = sender
        .send_request(request.body(Empty::<Bytes>::new())?)
        .await?;
    let status = response.status();
    let body = response.into_body().collect().await?.to_bytes();
    if !status.is_success() {
        return Err(format!("{url}: {status}").into());
    }
    print!("{}", String::from_utf8_lossy(&body));
    Ok(())
}

/// The HTTP proxy to reach `host` through, from `HTTP_PROXY` (or
/// `http_proxy`), unless `NO_PROXY` (or `no_proxy`) names the host.
fn proxy_for(host: &str) -> Option<(String, u16)> {
    let var = |name: &str| {
        std::env::var(name)
            .or_else(|_| std::env::var(name.to_lowercase()))
            .ok()
    };
    proxy_from(
        host,
        var("HTTP_PROXY")?.as_str(),
        var("NO_PROXY").as_deref(),
    )
}

/// The proxy of `proxy` (`http://host:port`) for `host`, given the `no_proxy`
/// list: `None` when the list names the host, or a domain it belongs to.
fn proxy_from(host: &str, proxy: &str, no_proxy: Option<&str>) -> Option<(String, u16)> {
    let excluded = no_proxy
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .any(|entry| {
            let domain = entry.trim_start_matches('.');
            entry == "*"
                || (!domain.is_empty() && (host == domain || host.ends_with(&format!(".{domain}"))))
        });
    if excluded {
        return None;
    }
    let proxy: Uri = proxy.parse().ok()?;
    Some((proxy.host()?.to_owned(), proxy.port_u16().unwrap_or(80)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_go_through_the_proxy_unless_listed_in_no_proxy() {
        let proxy = "http://gateway.docker.internal:3128";
        let no_proxy = Some("localhost,127.0.0.1,::1,gateway.docker.internal,.internal.example");
        assert_eq!(
            proxy_from("postman-echo.com", proxy, no_proxy),
            Some(("gateway.docker.internal".to_owned(), 3128))
        );
        assert_eq!(proxy_from("localhost", proxy, no_proxy), None);
        assert_eq!(proxy_from("api.internal.example", proxy, no_proxy), None);
        assert_eq!(proxy_from("postman-echo.com", proxy, Some("*")), None);
        assert_eq!(
            proxy_from("postman-echo.com", "http://proxy", None),
            Some(("proxy".to_owned(), 80))
        );
    }
}
