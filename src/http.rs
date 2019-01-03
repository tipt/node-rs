//! HTTP handling.

use hyper::header::{self, Headers};
use hyper::method::Method;
use hyper::mime::*;
use hyper::status::StatusCode;
use hyper::uri::RequestUri;
use hyper::version::HttpVersion;
use time::now_utc;
use tokio::io::write_all;
use tokio::net::TcpStream;
use tokio::prelude::*;
use websocket::server::upgrade::Request;

/// Handles a single HTTP request.
pub fn handle_http(stream: TcpStream, request: Request) {
    let addr = match stream.peer_addr() {
        Ok(addr) => addr,
        Err(_) => {
            info!("Rejecting HTTP request with no remote address");
            return;
        }
    };

    match request.subject {
        (method, RequestUri::AbsolutePath(path)) => match (method, &*path) {
            (Method::Post, "/api/forward") | (Method::Get, "/api/public_key") => {
                tokio::spawn(
                    write_html_error(stream, request.version, StatusCode::NotImplemented)
                        .map(|_| {})
                        .map_err(|_| {}),
                );
            }
            (m, p) => {
                info!("{}: not found: {} {}", addr, m, p);
                tokio::spawn(write_html_error(
                    stream,
                    request.version,
                    StatusCode::NotFound,
                ));
            }
        },
        (m, p) => {
            info!("{}: bad request: {} {}", addr, m, p);
            tokio::spawn(write_html_error(
                stream,
                request.version,
                StatusCode::BadRequest,
            ));
        }
    }
}

/// Writes a simple HTTP response with an HTML error page to the given AsyncWrite.
pub fn write_html_error<T: AsyncWrite>(
    stream: T,
    version: HttpVersion,
    status: StatusCode,
) -> impl Future<Item = (), Error = ()> {
    let server_name = format!("Tipt node-rs/{}", env!("CARGO_PKG_VERSION"));

    let html = format!(
        "<!DOCTYPE html>
<html>
    <head>
        <title>{0}</title>
        <meta charset='utf-8' />
    </head>
    <body style='font-family: monospace'>
        <center>
            <h1>{0}</h1>
            <hr>
            {1}
        </center>
    </body>
</html>",
        status, server_name
    );

    let mut response = Response::new(html.into());
    *response.version_mut() = version;
    *response.status_mut() = status;
    response
        .headers_mut()
        .set(header::ContentType(mime!(Text/Html; Charset=Utf8)));
    response.headers_mut().set(header::Server(server_name));

    response.write(stream).map(|_| {}).map_err(|_| {})
}

/// A simple HTTP response.
pub struct Response {
    body: Vec<u8>,
    version: HttpVersion,
    status: StatusCode,
    headers: Headers,
}

impl Response {
    /// Creates a new HTTP response with the given body.
    pub fn new(body: Vec<u8>) -> Response {
        Response {
            body,
            version: HttpVersion::Http11,
            status: StatusCode::Ok,
            headers: Headers::new(),
        }
    }

    /// Returns a mutable reference to the version.
    pub fn version_mut(&mut self) -> &mut HttpVersion {
        &mut self.version
    }

    /// Returns a mutable reference to the status code.
    pub fn status_mut(&mut self) -> &mut StatusCode {
        &mut self.status
    }

    /// Returns a mutable reference to the headers.
    pub fn headers_mut(&mut self) -> &mut Headers {
        &mut self.headers
    }

    /// Writes the response to the given stream and returns a future.
    pub fn write<W: AsyncWrite>(mut self, stream: W) -> impl Future {
        if !self.headers.has::<header::Date>() {
            self.headers.set(header::Date(header::HttpDate(now_utc())));
        }

        self.headers
            .set(header::ContentLength(self.body.len() as u64));

        let headers = self.headers;
        let body = self.body;

        write_all(stream, format!("{} {}\r\n", self.version, self.status))
            .and_then(move |(stream, _)| write_all(stream, format!("{}\r\n", headers)))
            .and_then(|(stream, _)| write_all(stream, body))
    }
}
