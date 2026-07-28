//! Agent ↔ daemon IPC protocol.
//!
//! Framing: `u32 BE length` + `u8 type` + payload.
//! Control messages use JSON payloads; stream data is raw bytes.
//!
//! Pair codes must never appear in any message.

use std::io;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const PROTOCOL_VERSION: u32 = 1;

/// Abstract Unix socket name used when `/run` is unavailable.
pub const DEFAULT_CONTROL_ABSTRACT: &str = "adb-hubd";

/// Filesystem control socket path when the daemon can manage `/run`.
pub const DEFAULT_CONTROL_PATH: &str = "/run/adb-hubd/control.sock";

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MsgType {
    RegisterAgent = 1,
    DeviceSnapshot = 2,
    DeviceSnapshotChanged = 3,
    OpenPrivateStream = 4,
    OpenResult = 5,
    StreamData = 6,
    StreamClose = 7,
    Ping = 8,
    Pong = 9,
}

impl MsgType {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::RegisterAgent),
            2 => Some(Self::DeviceSnapshot),
            3 => Some(Self::DeviceSnapshotChanged),
            4 => Some(Self::OpenPrivateStream),
            5 => Some(Self::OpenResult),
            6 => Some(Self::StreamData),
            7 => Some(Self::StreamClose),
            8 => Some(Self::Ping),
            9 => Some(Self::Pong),
            _ => None,
        }
    }
}

/// Sanitized device metadata reported by an agent (no pair codes).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SanitizedDevice {
    pub public_serial: String,
    pub upstream_serial: String,
    pub state: String,
    #[serde(default)]
    pub extras: String,
    pub backend_name: String,
    /// Opaque to the daemon; agent maps this to (addr, pair_code, upstream).
    pub route_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RegisterAgent {
    pub version: u32,
    #[serde(default)]
    pub capabilities: Vec<String>,
    pub instance_token: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceSnapshotMsg {
    pub generation: u64,
    pub devices: Vec<SanitizedDevice>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OpenPrivateStream {
    pub stream_id: u32,
    pub route_id: String,
    pub service: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OpenResult {
    pub stream_id: u32,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fail_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StreamClose {
    pub stream_id: u32,
    #[serde(default)]
    pub reason: String,
}

#[derive(Clone, Debug)]
pub enum IpcMessage {
    RegisterAgent(RegisterAgent),
    DeviceSnapshot(DeviceSnapshotMsg),
    DeviceSnapshotChanged(DeviceSnapshotMsg),
    OpenPrivateStream(OpenPrivateStream),
    OpenResult(OpenResult),
    /// stream_id + raw payload bytes
    StreamData { stream_id: u32, data: Vec<u8> },
    StreamClose(StreamClose),
    Ping,
    Pong,
}

pub async fn write_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    msg: &IpcMessage,
) -> io::Result<()> {
    let (ty, payload) = encode_message(msg)?;
    let len = (1 + payload.len()) as u32;
    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(&[ty as u8]).await?;
    writer.write_all(&payload).await?;
    writer.flush().await
}

pub async fn read_message<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<IpcMessage> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "empty IPC frame",
        ));
    }
    if len > 16 * 1024 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("IPC frame too large: {len}"),
        ));
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await?;
    let ty = MsgType::from_u8(body[0]).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown IPC type {}", body[0]),
        )
    })?;
    decode_message(ty, &body[1..])
}

fn encode_message(msg: &IpcMessage) -> io::Result<(MsgType, Vec<u8>)> {
    match msg {
        IpcMessage::RegisterAgent(m) => Ok((MsgType::RegisterAgent, to_json(m)?)),
        IpcMessage::DeviceSnapshot(m) => Ok((MsgType::DeviceSnapshot, to_json(m)?)),
        IpcMessage::DeviceSnapshotChanged(m) => Ok((MsgType::DeviceSnapshotChanged, to_json(m)?)),
        IpcMessage::OpenPrivateStream(m) => Ok((MsgType::OpenPrivateStream, to_json(m)?)),
        IpcMessage::OpenResult(m) => Ok((MsgType::OpenResult, to_json(m)?)),
        IpcMessage::StreamData { stream_id, data } => {
            let mut payload = Vec::with_capacity(4 + data.len());
            payload.extend_from_slice(&stream_id.to_be_bytes());
            payload.extend_from_slice(data);
            Ok((MsgType::StreamData, payload))
        }
        IpcMessage::StreamClose(m) => Ok((MsgType::StreamClose, to_json(m)?)),
        IpcMessage::Ping => Ok((MsgType::Ping, Vec::new())),
        IpcMessage::Pong => Ok((MsgType::Pong, Vec::new())),
    }
}

fn decode_message(ty: MsgType, payload: &[u8]) -> io::Result<IpcMessage> {
    match ty {
        MsgType::RegisterAgent => Ok(IpcMessage::RegisterAgent(from_json(payload)?)),
        MsgType::DeviceSnapshot => Ok(IpcMessage::DeviceSnapshot(from_json(payload)?)),
        MsgType::DeviceSnapshotChanged => {
            Ok(IpcMessage::DeviceSnapshotChanged(from_json(payload)?))
        }
        MsgType::OpenPrivateStream => Ok(IpcMessage::OpenPrivateStream(from_json(payload)?)),
        MsgType::OpenResult => Ok(IpcMessage::OpenResult(from_json(payload)?)),
        MsgType::StreamData => {
            if payload.len() < 4 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "StreamData too short",
                ));
            }
            let mut id_buf = [0u8; 4];
            id_buf.copy_from_slice(&payload[..4]);
            let stream_id = u32::from_be_bytes(id_buf);
            Ok(IpcMessage::StreamData {
                stream_id,
                data: payload[4..].to_vec(),
            })
        }
        MsgType::StreamClose => Ok(IpcMessage::StreamClose(from_json(payload)?)),
        MsgType::Ping => Ok(IpcMessage::Ping),
        MsgType::Pong => Ok(IpcMessage::Pong),
    }
}

fn to_json<T: Serialize>(v: &T) -> io::Result<Vec<u8>> {
    serde_json::to_vec(v).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn from_json<T: for<'de> Deserialize<'de>>(payload: &[u8]) -> io::Result<T> {
    serde_json::from_slice(payload).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Convert sanitized agent devices into registry entries (no pair codes).
pub fn sanitized_to_snapshot(
    devices: &[SanitizedDevice],
) -> crate::registry::DeviceSnapshot {
    use crate::registry::DeviceEntry;
    crate::registry::DeviceSnapshot {
        devices: devices
            .iter()
            .map(|d| DeviceEntry {
                public_serial: d.public_serial.clone(),
                upstream_serial: d.upstream_serial.clone(),
                state: d.state.clone(),
                extras: d.extras.clone(),
                backend_name: d.backend_name.clone(),
                // Placeholder; private streams go through the agent, not this addr.
                backend_addr: "127.0.0.1:0".parse().expect("valid"),
                pair_code: None,
                route_id: Some(d.route_id.clone()),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn roundtrip_register_and_stream() {
        let mut buf = Vec::new();
        write_message(
            &mut buf,
            &IpcMessage::RegisterAgent(RegisterAgent {
                version: PROTOCOL_VERSION,
                capabilities: vec!["devices".into()],
                instance_token: "tok".into(),
            }),
        )
        .await
        .unwrap();
        write_message(
            &mut buf,
            &IpcMessage::StreamData {
                stream_id: 7,
                data: b"hello".to_vec(),
            },
        )
        .await
        .unwrap();

        let mut cursor = std::io::Cursor::new(buf);
        match read_message(&mut cursor).await.unwrap() {
            IpcMessage::RegisterAgent(r) => {
                assert_eq!(r.version, PROTOCOL_VERSION);
                assert_eq!(r.instance_token, "tok");
            }
            other => panic!("unexpected {other:?}"),
        }
        match read_message(&mut cursor).await.unwrap() {
            IpcMessage::StreamData { stream_id, data } => {
                assert_eq!(stream_id, 7);
                assert_eq!(data, b"hello");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn sanitized_has_no_pair_code_field_in_json() {
        let d = SanitizedDevice {
            public_serial: "A".into(),
            upstream_serial: "A".into(),
            state: "device".into(),
            extras: String::new(),
            backend_name: "office".into(),
            route_id: "r1".into(),
        };
        let json = serde_json::to_string(&d).unwrap();
        assert!(!json.contains("pair"));
        assert!(json.contains("route_id"));
    }
}
