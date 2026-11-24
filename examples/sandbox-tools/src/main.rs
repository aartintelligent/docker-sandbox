//! `example-sandbox-tools`: the programs the examples run inside a sandbox.
//!
//! A sandbox runs Linux, whatever the host, and an example checks what
//! happens in there: a page served on a published port, the secrets the proxy
//! of the sandbox writes into requests. This binary does both, built
//! statically for the sandbox and copied into it by `common/src/program.rs`:
//!
//! - `serve <port> <directory>` serves the files of `<directory>` on `<port>`
//!   of every interface, and prints `listening on <port>` once it does;
//! - `get <url> [<header> <value>]...` sends `GET <url>`, plain HTTP, with the
//!   given headers, through the proxy of the sandbox (`HTTP_PROXY`, unless
//!   `NO_PROXY` names the host), and prints the body of a successful answer.
//!
//! Failures exit with code 1 and a message on standard error.
//!
//! ```sh
//! example-sandbox-tools serve 8000 /tmp/www
//! example-sandbox-tools get http://postman-echo.com/headers Authorization "Bearer …"
//! ```

#![allow(
    clippy::print_stdout,
    reason = "a command-line tool reports on standard output"
)]

use std::convert::Infallible;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper::header::{HOST, USER_AGENT};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode, Uri};
use hyper_util::rt::TokioIo;
use tokio::net::{TcpListener, TcpStream};

/// Usage, printed for unknown arguments.
const USAGE: &str =
    "usage: example-sandbox-tools serve <port> <directory> | get <url> [<header> <value>]...";

/// User agent of the requests: some services refuse anonymous clients.
const AGENT: &str = "docker-sandbox-examples";

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.split_first() {
        Some((command, [port, dir])) if command == "serve" => serve(port, dir).await,
        Some((command, [url, headers @ ..])) if command == "get" && headers.len() % 2 == 0 => {
            let headers: Vec<(&str, &str)> = headers
                .chunks_exact(2)
                .map(|pair| (pair[0].as_str(), pair[1].as_str()))
                .collect();
            get(url, &headers).await
        }
        _ => bail!(USAGE),
    }
}

/// Serves the files of `dir` on `port` until the process is stopped.
async fn serve(port: &str, dir: &str) -> Result<()> {
    let root = Arc::new(PathBuf::from(dir));
    let port: u16 = port.parse().context("the port is a number")?;
    let listener = TcpListener::bind(("0.0.0.0", port)).await?;
    println!("listening on {port}");
    loop {
        let (stream, _) = listener.accept().await?;
        let root = Arc::clone(&root);
        tokio::spawn(async move {
            let service = service_fn(move |request| answer(Arc::clone(&root), request));
            // A client that goes away ends its connection, not the server.
            let _ = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
    }
}

/// Answers `request` with the file of `root` it names, or `404 Not Found`.
async fn answer(
    root: Arc<PathBuf>,
    request: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let read = match file_for(&root, request.uri().path()) {
        Some(file) => tokio::fs::read(file).await.ok(),
        None => None,
    };
    let (status, body) = match read {
        Some(contents) => (StatusCode::OK, contents),
        None => (StatusCode::NOT_FOUND, b"not found\n".to_vec()),
    };
    let mut response = Response::new(Full::new(Bytes::from(body)));
    *response.status_mut() = status;
    Ok(response)
}

/// The file of `root` that the request path `path` names, `index.html` for a
/// directory; `None` for a path that would leave `root`.
fn file_for(root: &Path, path: &str) -> Option<PathBuf> {
    let mut file = root.to_path_buf();
    for component in Path::new(path.trim_start_matches('/')).components() {
        match component {
            Component::Normal(part) => file.push(part),
            Component::CurDir => {}
            _ => return None,
        }
    }
    if path.ends_with('/') {
        file.push("index.html");
    }
    Some(file)
}

/// Sends `GET url` with `headers`, and prints the body of a successful answer.
async fn get(url: &str, headers: &[(&str, &str)]) -> Result<()> {
    let uri: Uri = url.parse()?;
    if uri.scheme_str() != Some("http") {
        bail!("{url}: only http:// URLs are supported");
    }
    let host = uri.host().context("the URL has no host")?;
    let port = uri.port_u16().unwrap_or(80);
    // Through a proxy, the request names the whole URL; directly, its path.
    let ((address, address_port), target) = match proxy_for(host) {
        Some(proxy) => (proxy, uri.to_string()),
        None => (
            (host.to_owned(), port),
            uri.path_and_query()
                .map_or_else(|| "/".to_owned(), ToString::to_string),
        ),
    };
    let stream = TcpStream::connect((address.as_str(), address_port)).await?;
    let (mut sender, connection) =
        hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
    tokio::spawn(connection);

    let authority = uri.authority().map_or(host, |a| a.as_str());
    let mut request = Request::get(target)
        .header(HOST, authority)
        .header(USER_AGENT, AGENT);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = sender
        .send_request(request.body(Empty::<Bytes>::new())?)
        .await?;
    let status = response.status();
    let body = response.into_body().collect().await?.to_bytes();
    if !status.is_success() {
        bail!("{url}: {status}");
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
    fn paths_stay_inside_the_served_directory() {
        let root = Path::new("/tmp/www");
        assert_eq!(file_for(root, "/"), Some(root.join("index.html")));
        assert_eq!(file_for(root, "/docs/"), Some(root.join("docs/index.html")));
        assert_eq!(file_for(root, "/a.html"), Some(root.join("a.html")));
        assert_eq!(file_for(root, "/../etc/passwd"), None);
    }

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
