use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// ADB smart-socket: 4 ASCII hex digits length + payload.
pub async fn read_packet<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let len = parse_hex4(&len_buf).map_err(|msg| io::Error::new(io::ErrorKind::InvalidData, msg))?;
    let mut payload = vec![0u8; len];
    if len > 0 {
        reader.read_exact(&mut payload).await?;
    }
    Ok(payload)
}

pub async fn write_packet<W: AsyncWrite + Unpin>(writer: &mut W, payload: &[u8]) -> io::Result<()> {
    let header = format!("{:04x}", payload.len());
    writer.write_all(header.as_bytes()).await?;
    writer.write_all(payload).await?;
    writer.flush().await
}

pub async fn write_service<W: AsyncWrite + Unpin>(writer: &mut W, service: &str) -> io::Result<()> {
    write_packet(writer, service.as_bytes()).await
}

pub async fn read_status<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<[u8; 4]> {
    let mut status = [0u8; 4];
    reader.read_exact(&mut status).await?;
    Ok(status)
}

pub async fn write_okay<W: AsyncWrite + Unpin>(writer: &mut W) -> io::Result<()> {
    writer.write_all(b"OKAY").await?;
    writer.flush().await
}

pub async fn write_fail<W: AsyncWrite + Unpin>(writer: &mut W, reason: &str) -> io::Result<()> {
    writer.write_all(b"FAIL").await?;
    write_packet(writer, reason.as_bytes()).await
}

/// OKAY + length-prefixed body (devices list, features, etc.).
pub async fn write_okay_payload<W: AsyncWrite + Unpin>(
    writer: &mut W,
    payload: &[u8],
) -> io::Result<()> {
    write_okay(writer).await?;
    write_packet(writer, payload).await
}

/// After OKAY, read a length-prefixed string payload.
pub async fn read_okay_payload<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<Vec<u8>> {
    let status = read_status(reader).await?;
    match &status {
        b"OKAY" => read_packet(reader).await,
        b"FAIL" => {
            let reason = read_packet(reader).await?;
            let msg = String::from_utf8_lossy(&reason).into_owned();
            Err(io::Error::new(io::ErrorKind::Other, msg))
        }
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unexpected status: {:?}", std::str::from_utf8(other).unwrap_or("?")),
        )),
    }
}

pub fn parse_hex4(buf: &[u8; 4]) -> Result<usize, String> {
    let s = std::str::from_utf8(buf).map_err(|_| "hex4 is not utf8".to_string())?;
    usize::from_str_radix(s, 16).map_err(|_| format!("invalid hex4 length: {s}"))
}

pub fn encode_hex4(len: usize) -> String {
    format!("{:04x}", len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    #[tokio::test]
    async fn roundtrip_packet() {
        let mut buf = Vec::new();
        write_packet(&mut buf, b"host:devices").await.unwrap();
        assert_eq!(&buf[..4], b"000c");
        assert_eq!(&buf[4..], b"host:devices");

        let mut reader = BufReader::new(&buf[..]);
        let payload = read_packet(&mut reader).await.unwrap();
        assert_eq!(payload, b"host:devices");
    }

    #[tokio::test]
    async fn fail_includes_reason() {
        let mut buf = Vec::new();
        write_fail(&mut buf, "no devices").await.unwrap();
        assert_eq!(&buf[..4], b"FAIL");
        assert_eq!(&buf[4..8], b"000a");
        assert_eq!(&buf[8..], b"no devices");
    }
}
