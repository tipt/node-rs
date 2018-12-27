//! Tipt protocols.

use byteorder::{ReadBytesExt, BE};
use semver::Version;
use serde::de::{self, Error, Unexpected};
use serde::ser::SerializeStruct;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_derive::{Deserialize, Serialize};
use std::io::{self, Seek, SeekFrom};

const MAX_PACKET_SIZE: usize = 2_100_000;
const MAX_FORWARD_MSG_SIZE: usize = 2_000_000;

/// Creates a repr(primitive) enum with ser/de implementations.
macro_rules! enum_primitive {
    (
        $(#[$meta:meta])* pub $name:ident ($ty:tt)
        { $($(#[$vmeta:meta])* $variant:ident = $value:expr,)+ }
    ) => {
        enum_primitive!(__ $($meta)*; pub; $name; $ty; $($($vmeta)*; $variant, $value),+);
    };
    (
        __ $($meta:meta)*; $prefix:tt; $name:ident; $ty:tt;
        $($($vmeta:meta)*; $variant:ident, $value:expr),+
    ) => {
        $(#[$meta])*
        #[repr($ty)]
        #[derive(Debug, Clone, Copy, PartialEq)]
        $prefix enum $name {
            $($(#[$vmeta])* $variant = $value,)+
        }

        impl $name {
            pub fn from_raw(value: $ty) -> Option<$name> {
                match value {
                    $(
                        $value => Some($name::$variant),
                    )+
                    _ => None,
                }
            }
        }

        impl Into<$ty> for $name {
            fn into(self) -> $ty {
                self as $ty
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer
            {
                (*self as $ty).serialize(serializer)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                match $ty::deserialize(deserializer) {
                    Ok(val) => match Self::from_raw(val) {
                        Some(val) => Ok(val),
                        None => Err(D::Error::invalid_value(
                            Unexpected::Unsigned(val as u64),
                            &concat!("a ", stringify!($name)),
                        )),
                    },
                    Err(err) => Err(err),
                }
            }
        }
    };
}

enum_primitive! {
    /// The client-node packet type.
    pub CNPacketType (u8) {
        /// Indicates a request and demands a response.
        Request = 0x00,

        /// Indicates a response to a request.
        Response = 0x01,

        /// Sent when one party has information for the other party which is not sent as part
        /// of a response. No response is needed.
        Event = 0x02,
    }
}

enum_primitive! {
    /// The request type for requests sent *to* a node.
    pub CNNodeRequestType (u8) {
        /// The client wants to ensure that it’s compatible with the client-node protocol version
        /// the server is running.
        Handshake = 0x00,

        /// The user wants register on the node.
        Register = 0x01,

        /// The user wants to sign in.
        SignIn = 0x02,

        /// Obtains information about the node.
        GetInfo = 0x03,

        /// Obtains a node’s OpenPGP public key.
        GetNodeKey = 0x20,

        /// The client requests that a message be forwarded to another node.
        Forward = 0x21,
    }
}

impl CNNodeRequestType {
    /// If true, requires the client to be signed out.
    pub fn requires_signed_out(self) -> bool {
        match self {
            CNNodeRequestType::Register | CNNodeRequestType::SignIn => true,
            _ => false,
        }
    }

    /// If true, requires the client to be signed in.
    pub fn requires_signed_in(self) -> bool {
        match self {
            CNNodeRequestType::GetNodeKey | CNNodeRequestType::Forward => true,
            _ => false,
        }
    }
}

/// A packet from a client.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientPacket {
    Request(u8, NodeRequest),
}

/// A request from a client.
#[derive(Debug, Clone, PartialEq)]
pub enum NodeRequest {
    Handshake {
        /// The client’s protocol version.
        version: Version,
    },
    GetNodeKey {
        /// The fully-qualified domain name of the node whose OpenPGP key is being requested.
        node: String,
    },
    Forward {
        /// The fully-qualified domain name of the node to forward the message to.
        node: String,

        /// Whether to wait for the node to respond and send the response back to the client.
        wait: bool,

        /// The encrypted CNNodeForwardRequest to send to the node.
        /// Must be encrypted using the recipient node’s OpenPGP public key.
        message: Vec<u8>,
    },
}

/// A packet from a node.
#[derive(Debug, Clone, PartialEq)]
pub enum NodePacket {
    Response(u8, Result<NodeResponse, NodeErrResponse>),
}

impl Serialize for NodePacket {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_struct("packet", 3)?;
        match self {
            NodePacket::Response(id, content) => {
                map.serialize_field("id", id)?;
                map.serialize_field("pakt", &CNPacketType::Response)?;
                match content {
                    Ok(content) => map.serialize_field("body", content)?,
                    Err(error) => map.serialize_field("err", error)?,
                }
            }
        }
        map.end()
    }
}

/// A response to a client.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum NodeResponse {
    Handshake {
        /// The server’s protocol version.
        #[serde(rename = "ver")]
        version: Version,

        /// Whether the node is compatible with the client version.
        #[serde(rename = "comp")]
        compatible: bool,
    },
    #[serde(skip)]
    Forward,
    ForwardWithData {
        /// The response from the recipient node.
        #[serde(rename = "resp")]
        response: Vec<u8>,
    },
    GetNodeKey {
        /// The requested node’s OpenPGP public key, un-armored.
        key: Vec<u8>,
    },
}

/// Error responses to a client.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum NodeErrResponse {}

/// Returns the size of the given msgpack value in bytes.
fn msgpack_value_size(cursor: &mut io::Cursor<&[u8]>) -> Result<usize, rmp_serde::decode::Error> {
    use rmp::{decode::read_marker, Marker};
    use rmp_serde::decode::Error;

    let start_pos = cursor.position();

    /// Moves the cursor forward by a certain about of bytes.
    macro_rules! skip {
        // skips a set amount of bytes
        ($n:expr) => {{
            cursor.seek(SeekFrom::Current($n)).map_err(Error::InvalidDataRead)?;
        }};

        // skips whatever length is given by the next few bytes
        (with $n:ident) => { skip!(with $n::<>) };
        (with $n:ident::<$($t:ty)*>) => {{
            let len = cursor.$n::<$($t)*>().map_err(Error::InvalidDataRead)?;
            cursor.seek(SeekFrom::Current(len as i64)).map_err(Error::InvalidDataRead)?;
        }};

        // skips n msgpack objects
        (deep $n:expr) => {{
            for _ in 0..$n {
                msgpack_value_size(cursor)?;
            }
        }};
    }

    match read_marker(cursor)? {
        Marker::FixPos(_) | Marker::FixNeg(_) | Marker::Null | Marker::True | Marker::False => (),
        Marker::U8 | Marker::I8 => skip!(1),
        Marker::U16 | Marker::I16 | Marker::FixExt1 => skip!(2),
        Marker::FixExt2 => skip!(3),
        Marker::U32 | Marker::I32 | Marker::F32 => skip!(4),
        Marker::FixExt4 => skip!(5),
        Marker::U64 | Marker::I64 | Marker::F64 => skip!(8),
        Marker::FixExt8 => skip!(9),
        Marker::FixExt16 => skip!(17),
        Marker::FixStr(len) => skip!(len as i64),
        Marker::Str8 | Marker::Bin8 => skip!(with read_u8),
        Marker::Str16 | Marker::Bin16 => skip!(with read_u16::<BE>),
        Marker::Str32 | Marker::Bin32 => skip!(with read_u32::<BE>),
        Marker::FixArray(len) => skip!(deep len),
        Marker::Array16 => {
            let len = cursor.read_u16::<BE>().map_err(Error::InvalidDataRead)?;
            skip!(deep len);
        }
        Marker::Array32 => {
            let len = cursor.read_u32::<BE>().map_err(Error::InvalidDataRead)?;
            skip!(deep len);
        }
        Marker::FixMap(len) => skip!(deep len * 2),
        Marker::Map16 => {
            let len = cursor.read_u16::<BE>().map_err(Error::InvalidDataRead)?;
            skip!(deep len * 2);
        }
        Marker::Map32 => {
            let len = cursor.read_u32::<BE>().map_err(Error::InvalidDataRead)?;
            skip!(deep len * 2);
        }
        Marker::Ext8 => {
            let len = cursor.read_u8().map_err(Error::InvalidDataRead)?;
            skip!(len as i64 + 1);
        }
        Marker::Ext16 => {
            let len = cursor.read_u16::<BE>().map_err(Error::InvalidDataRead)?;
            skip!(len as i64 + 1);
        }
        Marker::Ext32 => {
            let len = cursor.read_u32::<BE>().map_err(Error::InvalidDataRead)?;
            skip!(len as i64 + 1);
        }
        Marker::Reserved => return Err(Error::Uncategorized("unexpected reserved type".into())),
    }

    Ok((cursor.position() - start_pos) as usize)
}

impl ClientPacket {
    /// Deserializes a msgpack packet from a client.
    ///
    /// This is not implemented with serde because it requires msgpack-specific features.
    pub fn deserialize(data: &[u8]) -> Result<ClientPacket, rmp_serde::decode::Error> {
        if data.len() > MAX_PACKET_SIZE {
            return Err(rmp_serde::decode::Error::Uncategorized(
                "packet too large".into(),
            ));
        }

        use rmp::decode::*;
        use rmp_serde::Deserializer;

        let mut cursor = io::Cursor::new(data);

        let mut id = None;
        let mut packet_type = None;
        let mut req_type = None;
        let mut body = None;
        let mut err = None;

        let map_len = read_map_len(&mut cursor)? as usize;
        // a packet cannot have more than five fields
        if map_len > 5 {
            return Err(de::Error::invalid_length(
                map_len,
                &"expected map of length <= 5",
            ));
        }
        for _ in 0..map_len {
            // keys cannot be longer that four characters
            // (yes, this will conflate `id` with `id ` and `id  ` making the set of acceptable
            //  packets slightly larger than it should be, but I’d say the benefit of not allocating
            //  a Vec on the heap is greater)
            let mut key_buf = [b' '; 4];
            read_str(&mut cursor, &mut key_buf)?;

            match &key_buf {
                b"id  " => id = Some(u8::deserialize(&mut Deserializer::from_read(&mut cursor))?),
                b"pakt" => {
                    packet_type = Some(CNPacketType::deserialize(&mut Deserializer::from_read(
                        &mut cursor,
                    ))?)
                }
                // the following three fields have an undefined type at this point because `pakt`
                // may not have been read yet, so they will just be assigned raw buffers that can
                // be decoded later
                b"reqt" => {
                    req_type = {
                        let pos = cursor.position() as usize;
                        let len = msgpack_value_size(&mut cursor)?;
                        Some(&data[pos..pos + len])
                    }
                }
                b"body" => {
                    body = {
                        let pos = cursor.position() as usize;
                        let len = msgpack_value_size(&mut cursor)?;
                        Some(&data[pos..pos + len])
                    }
                }
                b"err " => {
                    err = {
                        let pos = cursor.position() as usize;
                        let len = msgpack_value_size(&mut cursor)?;
                        Some(&data[pos..pos + len])
                    }
                }
                field => return Err(de::Error::custom(format!("unknown field {:?}", field))),
            }
        }

        // `id`, `pakt`, and `reqt` are required fields
        for (i, name) in &[
            (id.is_none(), "id"),
            (packet_type.is_none(), "pakt"),
            (req_type.is_none(), "reqt"),
        ] {
            if *i {
                return Err(de::Error::missing_field(name));
            }
        }

        // checked above; can be unwrapped
        let id = id.unwrap();

        match packet_type.unwrap() {
            CNPacketType::Request => {
                // requests must have a body
                let body = match body {
                    Some(body) => body,
                    None => return Err(de::Error::missing_field("body")),
                };

                let req_type = CNNodeRequestType::deserialize(&mut Deserializer::from_slice(
                    req_type.unwrap(),
                ))?;

                match req_type {
                    CNNodeRequestType::Handshake => {
                        #[derive(Deserialize)]
                        struct Body {
                            ver: Version,
                        }
                        let body = Body::deserialize(&mut Deserializer::from_slice(body))?;

                        Ok(ClientPacket::Request(
                            id,
                            NodeRequest::Handshake { version: body.ver },
                        ))
                    }
                    CNNodeRequestType::GetNodeKey => {
                        #[derive(Deserialize)]
                        struct Body {
                            node: String,
                        }
                        let body = Body::deserialize(&mut Deserializer::from_slice(body))?;

                        if body.node.len() > 500 {
                            return Err(rmp_serde::decode::Error::Uncategorized(
                                "node name too long".into(),
                            ));
                        }

                        Ok(ClientPacket::Request(
                            id,
                            NodeRequest::GetNodeKey { node: body.node },
                        ))
                    }
                    CNNodeRequestType::Forward => {
                        #[derive(Deserialize)]
                        struct Body {
                            node: String,
                            wait: bool,
                            msg: Vec<u8>,
                        }
                        let body = Body::deserialize(&mut Deserializer::from_slice(body))?;

                        if body.node.len() > 500 {
                            return Err(rmp_serde::decode::Error::Uncategorized(
                                "node name too long".into(),
                            ));
                        }
                        if body.msg.len() > MAX_FORWARD_MSG_SIZE {
                            return Err(rmp_serde::decode::Error::Uncategorized(
                                "message too long".into(),
                            ));
                        }

                        Ok(ClientPacket::Request(
                            id,
                            NodeRequest::Forward {
                                node: body.node,
                                wait: body.wait,
                                message: body.msg,
                            },
                        ))
                    }
                    _ => unimplemented!("other request types"),
                }
            }
            CNPacketType::Response => unimplemented!("response"),
            CNPacketType::Event => unimplemented!("event"),
        }
    }
}
