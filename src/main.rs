#[macro_use]
extern crate log;

use clap::*;
use std::net::IpAddr;
use std::process::exit;
use tokio::io::write_all;
use tokio::prelude::*;
use tokio::reactor::Handle;
use tokio::runtime::Runtime;
use websocket::r#async::Server;
use websocket::server::InvalidConnection;

const DEFAULT_PORT: &str = "8478";

pub mod client;
pub mod protocol;

fn main() {
    let matches = app_from_crate!()
        .arg(
            Arg::with_name("verbose")
                .short("v")
                .long("verbose")
                .help("Enables verbose logging"),
        )
        .arg(
            Arg::with_name("host")
                .short("h")
                .long("host")
                .takes_value(true)
                .help("Sets the host address (default: 0.0.0.0)"),
        )
        .arg(
            Arg::with_name("port")
                .short("p")
                .long("port")
                .takes_value(true)
                .help(&format!("Sets the port (default: {})", DEFAULT_PORT)),
        )
        .get_matches();

    let host = matches.value_of("host").unwrap_or("0.0.0.0");
    let host: IpAddr = match host.parse() {
        Ok(host) => host,
        Err(_) => {
            eprintln!("Invalid host “{}”", host);
            exit(1)
        }
    };

    let port = matches.value_of("port").unwrap_or(DEFAULT_PORT);
    let port: u16 = match port.parse() {
        Ok(port) => port,
        Err(_) => {
            eprintln!("Invalid port “{}”", port);
            exit(1)
        }
    };
    let verbose = matches.is_present("verbose");

    fern::Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!(
                "{} [{}] [{}] {}",
                time::now().rfc3339(),
                record.level(),
                record.target(),
                message,
            ))
        })
        .level(if verbose {
            log::LevelFilter::Trace
        } else {
            log::LevelFilter::Info
        })
        .chain(std::io::stderr())
        .apply()
        .expect("Failed to initialize logger");

    let mut runtime = Runtime::new().expect("Failed to create runtime");

    runtime
        .block_on::<_, _, ()>(future::lazy(move || {
            let server = match Server::bind((host, port), &Handle::current()) {
                Ok(server) => server,
                Err(err) => {
                    eprintln!(
                        "Failed to bind websocket server to {}:{}: {}",
                        host, port, err
                    );
                    exit(1)
                }
            };

            info!("Listening on {}:{}", host, port);

            server
                .incoming()
                .then(|result| {
                    match result {
                        Ok(res) => Ok(Some(res)),
                        Err(InvalidConnection {
                            stream,
                            parsed,
                            buffer: _,
                            error: _,
                        }) => {
                            if let (Some(stream), Some(req)) = (stream, parsed) {
                                tokio::spawn(
                                    write_html_error(
                                        stream,
                                        req.version.as_ref(),
                                        "400 Bad Request",
                                    )
                                    .map(|_| {})
                                    .map_err(|_| { /* don’t care */ }),
                                );
                            } else {
                                debug!("ignoring invalid connection");
                            }
                            Ok(None)
                        }
                    }
                })
                .filter_map(|item| item)
                .for_each(|(upgrade, addr)| {
                    info!("Acceping websocket connection from {}", addr);
                    let f = upgrade
                        .accept()
                        .map_err(|err| {
                            error!("oh no websocket error: {:?}", err);
                        })
                        .and_then(|(client, _)| client::accept(client).map_err(|_| {}))
                        .map(|_| {});
                    tokio::spawn(f);
                    Ok(())
                })
        }))
        .expect("WebSocket server died");
}

/// Writes a simple HTTP response with an HTML error page to the given AsyncWrite.
///
/// - `version`: the HTTP version; something like `HTTP/1.1`
/// - `error`: the HTTP error (such as `404 Not Found`), must be valid
fn write_html_error<T: AsyncWrite>(stream: T, version: &str, error: &str) -> impl Future {
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
        error, server_name
    );

    let response = format!(
        "\
{} {}\r
Content-Type: text/html; charset=UTF-8\r
Content-Length: {}\r
Server: {}\r
\r
{}",
        version,
        error,
        html.len(),
        server_name,
        html
    );

    write_all(stream, response)
}
