//! Byte-level helpers used by P2P protocol messages.

use bytes::{BufMut, Bytes, BytesMut};

use crate::protocol::ProtoError;

pub fn write_u8_string(buf: &mut BytesMut, s: &str) {
    let bytes = s.as_bytes();
    let len = bytes.len().min(u8::MAX as usize);
    buf.put_u8(len as u8);
    buf.extend_from_slice(&bytes[..len]);
}

pub fn read_u8_string_at(frame: &Bytes, pos: &mut usize) -> Result<String, ProtoError> {
    if *pos >= frame.len() {
        return Err(ProtoError::TooShort(0));
    }
    let len = frame[*pos] as usize;
    *pos += 1;
    if frame.len().saturating_sub(*pos) < len {
        return Err(ProtoError::BadLength);
    }
    let s = String::from_utf8(frame[*pos..*pos + len].to_vec())?;
    *pos += len;
    Ok(s)
}

pub fn write_fixed_bytes(buf: &mut BytesMut, src: &[u8]) {
    buf.extend_from_slice(src);
}

pub fn read_fixed_bytes_at<const N: usize>(
    frame: &Bytes,
    pos: &mut usize,
) -> Result<[u8; N], ProtoError> {
    if frame.len().saturating_sub(*pos) < N {
        return Err(ProtoError::TooShort(frame.len().saturating_sub(*pos)));
    }
    let mut out = [0u8; N];
    out.copy_from_slice(&frame[*pos..*pos + N]);
    *pos += N;
    Ok(out)
}

pub fn read_u8_at(frame: &Bytes, pos: &mut usize) -> Result<u8, ProtoError> {
    if *pos >= frame.len() {
        return Err(ProtoError::TooShort(0));
    }
    let v = frame[*pos];
    *pos += 1;
    Ok(v)
}

pub fn read_i8_at(frame: &Bytes, pos: &mut usize) -> Result<i8, ProtoError> {
    Ok(read_u8_at(frame, pos)? as i8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BytesMut;

    #[test]
    fn u8_string_round_trip() {
        let mut buf = BytesMut::new();
        write_u8_string(&mut buf, "hello");
        let frozen = buf.freeze();
        let mut pos = 0;
        let out = read_u8_string_at(&frozen, &mut pos).unwrap();
        assert_eq!(out, "hello");
        assert_eq!(pos, 6);
    }

    #[test]
    fn fixed_bytes_round_trip() {
        let mut buf = BytesMut::new();
        let src = [42u8; 16];
        write_fixed_bytes(&mut buf, &src);
        let frozen = buf.freeze();
        let mut pos = 0;
        let out: [u8; 16] = read_fixed_bytes_at(&frozen, &mut pos).unwrap();
        assert_eq!(out, src);
        assert_eq!(pos, 16);
    }

    #[test]
    fn read_too_short_errors() {
        let frozen = bytes::Bytes::from_static(&[1, 2]);
        let mut pos = 0;
        let r: Result<[u8; 16], _> = read_fixed_bytes_at(&frozen, &mut pos);
        assert!(r.is_err());
    }
}
