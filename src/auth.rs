use std::io;

use tokio::net::TcpStream;

use crate::protocol::{read_status, write_fail, write_okay, write_service};

pub const PAIR_CODE_LEN: usize = 8;
pub const AUTH_PREFIX: &str = "auth:";

const PAIR_ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

/// Generate an 8-character alphanumeric pair code (A-Z0-9).
pub fn generate_pair_code() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    (0..PAIR_CODE_LEN)
        .map(|_| {
            let idx = rng.gen_range(0..PAIR_ALPHABET.len());
            PAIR_ALPHABET[idx] as char
        })
        .collect()
}

/// Validate that `code` is exactly 8 chars from A-Z0-9.
pub fn validate_pair_code(code: &str) -> Result<(), String> {
    if code.len() != PAIR_CODE_LEN {
        return Err(format!(
            "pair code must be {PAIR_CODE_LEN} characters, got {}",
            code.len()
        ));
    }
    if !code
        .bytes()
        .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
    {
        return Err("pair code must be alphanumeric A-Z0-9".into());
    }
    Ok(())
}

pub fn auth_service(code: &str) -> String {
    format!("{AUTH_PREFIX}{code}")
}

/// Parse `auth:XXXXXXXX` and return the code, or an error reason.
pub fn parse_auth_service(service: &str) -> Result<&str, String> {
    let Some(code) = service.strip_prefix(AUTH_PREFIX) else {
        return Err("expected auth:<pair-code> as first packet".into());
    };
    validate_pair_code(code)?;
    Ok(code)
}

/// Check client-provided code against the proxy's expected code.
pub fn codes_match(expected: &str, provided: &str) -> bool {
    expected == provided
}

/// Hub → proxy: send auth frame and require OKAY.
pub async fn authenticate_stream(stream: &mut TcpStream, code: &str) -> io::Result<()> {
    write_service(stream, &auth_service(code)).await?;
    let status = read_status(stream).await?;
    match &status {
        b"OKAY" => Ok(()),
        b"FAIL" => {
            let reason = crate::protocol::read_packet(stream).await?;
            let msg = String::from_utf8_lossy(&reason).into_owned();
            Err(io::Error::new(io::ErrorKind::PermissionDenied, msg))
        }
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "unexpected auth status: {:?}",
                std::str::from_utf8(other).unwrap_or("?")
            ),
        )),
    }
}

/// Proxy side: validate first packet; write OKAY or FAIL.
pub async fn accept_auth(stream: &mut TcpStream, expected: &str) -> io::Result<bool> {
    let payload = match crate::protocol::read_packet(stream).await {
        Ok(p) => p,
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => {
            write_fail(stream, "missing auth").await?;
            return Ok(false);
        }
        Err(err) => return Err(err),
    };

    let service = match String::from_utf8(payload) {
        Ok(s) => s,
        Err(_) => {
            write_fail(stream, "invalid auth utf8").await?;
            return Ok(false);
        }
    };

    match parse_auth_service(&service) {
        Ok(code) if codes_match(expected, code) => {
            write_okay(stream).await?;
            Ok(true)
        }
        Ok(_) => {
            write_fail(stream, "unauthorized").await?;
            Ok(false)
        }
        Err(reason) => {
            write_fail(stream, &reason).await?;
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_has_correct_shape() {
        let code = generate_pair_code();
        assert!(validate_pair_code(&code).is_ok());
    }

    #[test]
    fn reject_bad_codes() {
        assert!(validate_pair_code("SHORT").is_err());
        assert!(validate_pair_code("abcd1234").is_err());
        assert!(validate_pair_code("ABCD12-4").is_err());
        assert!(validate_pair_code("ABCD1234").is_ok());
    }

    #[test]
    fn parse_auth() {
        assert_eq!(parse_auth_service("auth:ABCD1234").unwrap(), "ABCD1234");
        assert!(parse_auth_service("host:devices").is_err());
    }
}
