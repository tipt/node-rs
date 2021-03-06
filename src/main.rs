#[macro_use]
extern crate log;

use clap::*;
use hyper::method::Method;
use hyper::uri::RequestUri;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::process::exit;
use tokio::prelude::*;
use tokio::reactor::Handle;
use tokio::runtime::Runtime;
use websocket::header::Headers;
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
        .arg(
            Arg::with_name("proxy")
                .short("P")
                .long("proxy")
                .help("Set to prefer the X-Real-IP header for obtaining client addresses\n\
                       (note that this can be spoofed if the client is connecting directly)"),
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

    let proxy = matches.is_present("proxy");

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
                .then(move |result| match result {
                    Ok(res) => Ok(Some(res)),
                    Err(InvalidConnection {
                        stream,
                        parsed,
                        buffer: _,
                        error: _,
                    }) => {
                        if let (Some(stream), None) = (&stream, &parsed) {
                            match stream.peer_addr() {
                                Ok(addr) => info!("Ignoring invalid connection from {}", addr),
                                Err(_) => {
                                    info!("Ignoring invalid connection from an unknown address");
                                }
                            }
                        } else if let (Some(stream), Some(req)) = (stream, parsed) {
                            match stream.peer_addr() {
                                Ok(addr) => {
                                    let addr = peer_addr(&req.headers, addr, proxy);
                                    http::handle_http(stream, req, addr);
                                }
                                Err(_) => {
                                    info!("Ignoring invalid connection from an unknown address");
                                }
                            };
                        } else {
                            info!("Ignoring invalid connection from an unknown address");
                        }
                        Ok(None)
                    }
                })
                .filter_map(|item| item)
                .for_each(move |(upgrade, addr)| {
                    let addr = peer_addr(&upgrade.headers, addr, proxy);

                    let accept = match &upgrade.request.subject {
                        (Method::Get, RequestUri::AbsolutePath(path)) => match &**path {
                            "/ws" => true,
                            path => {
                                info!(
                                    "Rejecting websocket connection from {} (bad path {})",
                                    addr, path
                                );
                                false
                            }
                        },
                        (m, p) => {
                            info!(
                                "Rejecting websocket connection from {} (bad request {} {})",
                                addr, m, p
                            );
                            false
                        }
                    };

                    if accept {
                        info!("Acceping websocket connection from {}", addr);
                        tokio::spawn(
                            upgrade
                                .accept()
                                .map_err(move |err| {
                                    error!(
                                        "Failed to accept websocket connection from {}: {}",
                                        addr, err
                                    );
                                })
                                .and_then(move |(client, _)| client::accept(client, addr)),
                        );
                    } else {
                        tokio::spawn(upgrade.reject().map(|_| {}).map_err(|_| {}));
                    }
                    Ok(())
                })
        }))
        .expect("WebSocket server died");
}

/// Resolves a peer address that may be behind a proxy, falling back to the given address otherwise.
fn peer_addr(headers: &Headers, addr: SocketAddr, proxy: bool) -> SocketAddr {
    if proxy {
        match headers.get_raw("x-real-ip") {
            Some(bufs) => {
                if let Some(buf) = bufs.get(0) {
                    match String::from_utf8_lossy(buf).parse() {
                        Ok(real_ip) => SocketAddr::new(real_ip, 0), // don’t know the port
                        Err(_) => addr,
                    }
                } else {
                    addr
                }
            }
            None => addr,
        }
    } else {
        addr
    }
}
