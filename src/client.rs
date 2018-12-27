//! Clients.

use futures::sync::mpsc;
use crate::protocol::{ClientPacket, NodeRequest, NodePacket, NodeResponse};
use parking_lot::Mutex;
use std::time::{Duration, Instant};
use tokio::codec::Framed;
use tokio::prelude::*;
use tokio::timer::Delay;
use websocket::futures::stream::Stream;
use websocket::r#async::{MessageCodec, TcpStream};
use websocket::{OwnedMessage, WebSocketError};
use semver::{Version, VersionReq};
use lazy_static::lazy_static;
use serde::Serialize;
use rmp_serde::Serializer;

const CLIENT_HANDSHAKE_TIMEOUT_SECS: u64 = 3;

lazy_static! {
    static ref SERVER_PROTOCOL_VERSION: Version = "1.0.0".parse().unwrap();
    static ref REQUIRED_PROTOCOL_VERSION: VersionReq = "1.0.0".parse().unwrap();
}

/// Accepts a websocket connection and creates a client if a Handshake message is received on time.
pub fn accept(socket: Framed<TcpStream, MessageCodec<OwnedMessage>>) -> impl Future {
    let f = socket
        .into_future()
        .map_err(|(err, _)| err)
        .and_then(move |(message, socket)| match message {
            Some(OwnedMessage::Binary(buf)) => match ClientPacket::deserialize(&buf) {
                Ok(ClientPacket::Request(id, NodeRequest::Handshake { version })) => {
                    let compatible = REQUIRED_PROTOCOL_VERSION.matches(&version);
                    let msg = NodePacket::Response(id, Ok(NodeResponse::Handshake {
                        version: SERVER_PROTOCOL_VERSION.clone(),
                        compatible,
                    }));

                    if REQUIRED_PROTOCOL_VERSION.matches(&version) {
                        let client = Client::new(socket);
                        client.send(msg);
                        future::Either::A(client)
                    } else {
                        let mut buf = Vec::new();
                        if let Err(err) = msg.serialize(&mut Serializer::new_named(&mut buf)) {
                            error!("Failed to serialize handshake response: {:?}", err);
                        } else {
                            tokio::spawn(socket.send(OwnedMessage::Binary(buf))
                                    .map(|_| {})
                                    .map_err(|_| {}));
                        }
                        future::Either::B(future::ok(()))
                    }
                }
                Ok(_) | Err(_) => {
                    future::Either::B(future::ok(()))
                },
            },
            Some(_) | None => future::Either::B(future::ok(())),
        })
        .map(|()| {})
        .map_err(|err| error!("websocket error: {:?}", err));

    let timeout = Instant::now() + Duration::from_secs(CLIENT_HANDSHAKE_TIMEOUT_SECS);
    Delay::new(timeout).map(|()| {}).map_err(|_| {}).join(f)
}

struct ClientState; // TODO: this

/// A websocket client.
///
/// This is a future that will resolve when the client disconnects.
pub struct Client {
    socket: Framed<TcpStream, MessageCodec<OwnedMessage>>,
    msg_queue: mpsc::UnboundedReceiver<NodePacket>,
    msg_queue_in: mpsc::UnboundedSender<NodePacket>,
    state: Mutex<ClientState>,
}

impl Client {
    fn new(socket: Framed<TcpStream, MessageCodec<OwnedMessage>>) -> Client {
        let (msg_queue_in, msg_queue) = mpsc::unbounded();

        Client {
            socket,
            msg_queue,
            msg_queue_in,
            state: Mutex::new(ClientState),
        }
    }

    pub fn send(&self, packet: NodePacket) {
        if let Err(_) = self.msg_queue_in.unbounded_send(packet) {
            error!("Failed to put packet in message queue");
        }
    }
}

impl Future for Client {
    type Item = ();
    type Error = WebSocketError;

    fn poll(&mut self) -> Poll<(), WebSocketError> {
        loop {
            match self.msg_queue.poll().unwrap() {
                Async::Ready(Some(msg)) => {
                    let mut buf = Vec::new();
                    if let Err(err) = msg.serialize(&mut Serializer::new_named(&mut buf)) {
                        error!("Failed to serialize response message: {:?}", err);
                    } else {
                        self.socket.start_send(OwnedMessage::Binary(buf))?;
                    }
                }
                _ => break,
            }
        }

        self.socket.poll_complete()?;

        while let Async::Ready(msg) = self.socket.poll()? {
            if let Some(msg) = msg {
                // TODO: handle
                unimplemented!("message received")
            } else {
                return Ok(Async::Ready(()));
            }
        }

        Ok(Async::NotReady)
    }
}
