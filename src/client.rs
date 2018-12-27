use crate::protocol::{ClientPacket, NodeRequest};
use parking_lot::Mutex;
use std::time::{Duration, Instant};
use tokio::codec::Framed;
use tokio::prelude::*;
use tokio::timer::Delay;
use websocket::futures::stream::Stream;
use websocket::r#async::{MessageCodec, TcpStream};
use websocket::{OwnedMessage, WebSocketError};

const CLIENT_HANDSHAKE_TIMEOUT_SECS: u64 = 3;
const REQUIRED_PROTOCOL_VERSION: &str = "1.0.0";

pub fn accept(socket: Framed<TcpStream, MessageCodec<OwnedMessage>>) -> impl Future {
    let f = socket
        .into_future()
        .map_err(|(err, _)| err)
        .and_then(move |(message, socket)| match message {
            Some(OwnedMessage::Binary(buf)) => match ClientPacket::deserialize(&buf) {
                Ok(ClientPacket::Request(NodeRequest::Handshake { id, version })) => {
                    future::Either::A(Client::new(socket))
                }
                Err(_) => future::Either::B(future::ok(())),
            },
            Some(_) | None => future::Either::B(future::ok(())),
        })
        .map(|()| {})
        .map_err(|err| error!("websocket error: {:?}", err));

    let timeout = Instant::now() + Duration::from_secs(CLIENT_HANDSHAKE_TIMEOUT_SECS);
    Delay::new(timeout).map(|()| {}).map_err(|_| {}).select(f)
}

struct ClientState; // TODO: this

/// A websocket client.
pub struct Client {
    socket: Framed<TcpStream, MessageCodec<OwnedMessage>>,
    state: Mutex<ClientState>,
}

impl Client {
    fn new(socket: Framed<TcpStream, MessageCodec<OwnedMessage>>) -> Client {
        Client {
            socket,
            state: Mutex::new(ClientState),
        }
    }
}

impl Future for Client {
    type Item = ();
    type Error = WebSocketError;

    fn poll(&mut self) -> Poll<(), WebSocketError> {
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
