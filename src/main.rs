#[macro_use]
extern crate log;

use clap::*;
use hyper::method::Method;
use hyper::uri::RequestUri;
use std::net::IpAddr;
use std::process::exit;
use tokio::prelude::*;
use tokio::reactor::Handle;
use tokio::runtime::Runtime;
use websocket::r#async::Server;
use websocket::server::InvalidConnection;

const DEFAULT_PORT: &str = "8478";

pub mod client;
pub mod http;
pub mod protocol;

fn main() {
    let matches = app_from_crate!()
        .arg(
            Arg::with_name("verbose")
                .short("v")
                .long("verbose")
                .multiple(true)
                .help("Enables debug/trace logging (repeat to increase verbosity)"),
        )
        .arg(
            Arg::with_name("host")
                .short("H")
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

    let (log_level, lib_log_level) = match matches.occurrences_of("verbose") {
        0 => (log::LevelFilter::Info, log::LevelFilter::Info),
        1 => (log::LevelFilter::Debug, log::LevelFilter::Debug),
        2 => (log::LevelFilter::Trace, log::LevelFilter::Debug),
        3 => (log::LevelFilter::Trace, log::LevelFilter::Trace),
        n => {
            eprintln!("No such verbosity level: {}", n);
            exit(1)
        }
    };

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
        .level(log_level)
        // set a different log level for some targets that’d spam stderr otherwise
        .level_for("tokio_threadpool", lib_log_level)
        .level_for("tokio_reactor", lib_log_level)
        .level_for("tokio_io", lib_log_level)
        .level_for("hyper", lib_log_level)
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
                .then(|result| match result {
                    Ok(res) => Ok(Some(res)),
                    Err(InvalidConnection {
                        stream,
                        parsed,
                        buffer: _,
                        error: _,
                    }) => {
                        if let (Some(stream), Some(req)) = (stream, parsed) {
                            http::handle_http(stream, req);
                        } else {
                            trace!("Ignoring invalid connection");
                        }
                        Ok(None)
                    }
                })
                .filter_map(|item| item)
                .for_each(|(upgrade, addr)| {
                    let accept = match &upgrade.request.subject {
                        (Method::Get, RequestUri::AbsolutePath(path)) => match &**path {
                            "/ws" => true,
                            path => {
                                debug!(
                                    "Rejecting websocket connection from {} (bad path {:?})",
                                    addr, path
                                );
                                false
                            }
                        },
                        (m, p) => {
                            debug!(
                                "Rejecting websocket connection from {} (bad request {:?} {:?})",
                                addr, m, p
                            );
                            false
                        }
                    };

                    if accept {
                        debug!("Acceping websocket connection from {}", addr);
                        tokio::spawn(
                            upgrade
                                .accept()
                                .map_err(move |err| {
                                    error!(
                                        "Failed to accept websocket connection from {}: {:?}",
                                        addr, err
                                    );
                                })
                                .and_then(|(client, _)| client::accept(client)),
                        );
                    } else {
                        tokio::spawn(upgrade.reject().map(|_| {}).map_err(|_| {}));
                    }
                    Ok(())
                })
        }))
        .expect("WebSocket server died");
}
