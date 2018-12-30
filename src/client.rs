//! Clients.

use crate::protocol::{
    ClientPacket, NodePacket, NodeRequest, NodeResponse, MAX_CLIENT_PACKET_SIZE,
};
use futures::sync::mpsc;
use lazy_static::lazy_static;
use parking_lot::Mutex;
use rmp_serde::Serializer;
use semver::{Version, VersionReq};
use serde::Serialize;
use std::time::{Duration, Instant};
use tokio::codec::Framed;
use tokio::prelude::*;
use tokio::timer::Delay;
use websocket::futures::stream::Stream;
use websocket::r#async::{MessageCodec, TcpStream};
use websocket::{CloseData, OwnedMessage, WebSocketError};

const CLIENT_HANDSHAKE_TIMEOUT_SECS: u64 = 3;

lazy_static! {
    static ref SERVER_PROTOCOL_VERSION: Version = "1.0.0".parse().unwrap();
    static ref REQUIRED_PROTOCOL_VERSION: VersionReq = "1.0.0".parse().unwrap();
}

/// Accepts a websocket connection and creates a client if a Handshake message is received on time.
pub fn accept(
    socket: Framed<TcpStream, MessageCodec<OwnedMessage>>,
) -> impl Future<Item = (), Error = ()> {
    let f = socket
        .into_future()
        .map_err(|(err, _)| err)
        .and_then(move |(message, socket)| match message {
            Some(OwnedMessage::Binary(buf)) => match ClientPacket::deserialize(&buf) {
                Ok(ClientPacket::Request(id, NodeRequest::Handshake { version })) => {
                    let compatible = REQUIRED_PROTOCOL_VERSION.matches(&version);

                    trace!(
                        "Got handshake from {:?} with version {}, compatible: {}",
                        socket.get_ref().peer_addr(),
                        version,
                        compatible
                    );

                    let msg = NodePacket::Response(
                        id,
                        Ok(NodeResponse::Handshake {
                            version: SERVER_PROTOCOL_VERSION.clone(),
                            compatible,
                        }),
                    );

                    if REQUIRED_PROTOCOL_VERSION.matches(&version) {
                        let client = Client::new(socket);
                        client.send(msg);
                        future::Either::A(client)
                    } else {
                        let mut buf = Vec::new();
                        if let Err(err) = msg.serialize(&mut Serializer::new_named(&mut buf)) {
                            error!("Failed to serialize handshake response: {:?}", err);
                        } else {
                            tokio::spawn(
                                socket
                                    .send(OwnedMessage::Binary(buf))
                                    .map(|_| {})
                                    .map_err(|_| {}),
                            );
                        }
                        future::Either::B(future::ok(()))
                    }
                }
                Ok(_) | Err(_) => future::Either::B(future::ok(())),
            },
            Some(_) | None => future::Either::B(future::ok(())),
        })
        .map(|()| {})
        .map_err(|err| error!("Websocket error: {:?}", err));

    let timeout = Instant::now() + Duration::from_secs(CLIENT_HANDSHAKE_TIMEOUT_SECS);
    Delay::new(timeout)
        .map(|()| {})
        .map_err(|_| {})
        .join(f)
        .map(|((), ())| ())
}

struct ClientState; // TODO: this

/// A websocket client.
///
/// This is a future that will resolve when the client disconnects.
pub struct Client {
    socket: Framed<TcpStream, MessageCodec<OwnedMessage>>,
    msg_queue: mpsc::UnboundedReceiver<OwnedMessage>,
    msg_queue_in: mpsc::UnboundedSender<OwnedMessage>,
    state: Mutex<ClientState>,
    closing: Option<CloseData>,
}

impl Client {
    fn new(socket: Framed<TcpStream, MessageCodec<OwnedMessage>>) -> Client {
        let (msg_queue_in, msg_queue) = mpsc::unbounded();

        Client {
            socket,
            msg_queue,
            msg_queue_in,
            state: Mutex::new(ClientState),
            closing: None,
        }
    }

    /// Returns true if this connection is considered closed and will not receive nor send any more
    /// messages.
    pub fn is_closed(&self) -> bool {
        self.closing.is_some()
    }

    /// Sends a websocket message.
    ///
    /// (actually just puts it in a queue)
    fn send_msg(&self, message: OwnedMessage) {
        if self.is_closed() {
            return;
        }

        if let Err(_) = self.msg_queue_in.unbounded_send(message) {
            error!("Failed to put message in client message queue");
        }
    }

    /// Sends a packet to the client.
    pub fn send(&self, packet: NodePacket) {
        let mut buf = Vec::new();
        if let Err(err) = packet.serialize(&mut Serializer::new_named(&mut buf)) {
            error!("Failed to serialize client packet: {:?}", err);
        } else {
            self.send_msg(OwnedMessage::Binary(buf));
        }
    }

    /// Marks this connection as closed with the given status and reason text.
    ///
    /// This will send a final close message through the websocket and resolve this future.
    /// Note that this will not actually forcefully close the connection until this struct is
    /// dropped.
    pub fn close(&mut self, status_code: u16, reason: String) {
        self.closing = Some(CloseData {
            status_code,
            reason,
        });
    }
}

impl Future for Client {
    type Item = ();
    type Error = WebSocketError;

    fn poll(&mut self) -> Poll<(), WebSocketError> {
        if let Some(close_data) = &self.closing {
            self.socket
                .start_send(OwnedMessage::Close(Some(close_data.clone())))?;
            return self.socket.poll_complete();
        }

        loop {
            match self.msg_queue.poll().unwrap() {
                Async::Ready(Some(msg)) => {
                    self.socket.start_send(msg)?;
                }
                _ => break,
            }
        }

        self.socket.poll_complete()?;

        while let Async::Ready(msg) = self.socket.poll()? {
            if let Some(msg) = msg {
                match msg {
                    OwnedMessage::Binary(buf) => {
                        if buf.len() > MAX_CLIENT_PACKET_SIZE {
                            self.close(
                                1009,
                                format!(
                                    "Packet too large (exceeds {} bytes)",
                                    MAX_CLIENT_PACKET_SIZE
                                ),
                            );
                            return Ok(Async::NotReady);
                        }

                        let packet = match ClientPacket::deserialize(&buf) {
                            Ok(packet) => packet,
                            Err(_) => unimplemented!("client packet parse error"),
                        };

                        unimplemented!("got client packet {:?}", packet)
                    }
                    OwnedMessage::Ping(payload) => {
                        self.send_msg(OwnedMessage::Pong(payload));
                    }
                    // TODO: handle
                    _ => unimplemented!("message received"),
                }
            } else {
                return Ok(Async::Ready(()));
            }
        }

        Ok(Async::NotReady)
    }
}
