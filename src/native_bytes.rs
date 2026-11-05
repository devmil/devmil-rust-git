//! Lossless conversion between native paths and the bytes Git reads and writes.
//!
//! There is exactly one encoding per platform:
//!
//! * **Unix**: the raw `OsStr` bytes. Git paths are bytes, so every name round-trips, including
//!   non-UTF-8 names.
//! * **Windows**: [WTF-8](https://simonsapin.github.io/wtf-8/). Git for Windows reads and writes
//!   paths as UTF-8, and WTF-8 is identical to UTF-8 for every well-formed name, so the bytes
//!   equal Git's own. WTF-8 additionally encodes unpaired UTF-16 surrogates, which Windows
//!   permits in file names, so conversion never loses information. UTF-16LE was rejected
//!   because its bytes differ from what Git prints and from what it accepts on standard input.
//!
//! Nothing here is lossy: converting bytes that are not valid for the platform is an error rather
//! than a replacement character. Use [`std::path::Path::display`] for presentation only.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// The bytes are not a valid native name on this platform (Windows: not WTF-8).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct NativeBytesError;

impl std::fmt::Display for NativeBytesError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("the bytes are not a valid native path on this platform")
    }
}

impl std::error::Error for NativeBytesError {}

/// Returns the platform's lossless byte form of `value`.
#[must_use]
pub fn os_str_to_bytes(value: &OsStr) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        value.as_bytes().to_vec()
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt as _;
        encode_wtf8(&value.encode_wide().collect::<Vec<_>>())
    }
    #[cfg(not(any(unix, windows)))]
    {
        value
            .to_str()
            .map_or_else(Vec::new, |text| text.as_bytes().to_vec())
    }
}

/// Returns the platform's lossless byte form of `path`.
#[must_use]
pub fn path_to_bytes(path: &Path) -> Vec<u8> {
    os_str_to_bytes(path.as_os_str())
}

/// Converts platform bytes back to a native string.
///
/// # Errors
/// On Windows, rejects bytes that are not WTF-8 (UTF-8 always succeeds). Never fails on Unix.
pub fn bytes_to_os_string(bytes: &[u8]) -> Result<OsString, NativeBytesError> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt as _;
        Ok(OsString::from_vec(bytes.to_vec()))
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt as _;
        decode_wtf8(bytes)
            .map(|units| OsString::from_wide(&units))
            .ok_or(NativeBytesError)
    }
    #[cfg(not(any(unix, windows)))]
    {
        std::str::from_utf8(bytes)
            .map(OsString::from)
            .map_err(|_| NativeBytesError)
    }
}

/// Converts platform bytes back to a native path.
///
/// # Errors
/// Same as [`bytes_to_os_string`].
pub fn bytes_to_path_buf(bytes: &[u8]) -> Result<PathBuf, NativeBytesError> {
    bytes_to_os_string(bytes).map(PathBuf::from)
}

/// Encodes UTF-16 code units as WTF-8. Exposed for platform-independent testing and for
/// consumers that receive UTF-16 from another runtime.
#[must_use]
pub fn encode_wtf8(units: &[u16]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(units.len());
    let mut index = 0;
    while index < units.len() {
        let unit = units[index];
        let code_point = if (0xD800..0xDC00).contains(&unit)
            && units
                .get(index + 1)
                .is_some_and(|next| (0xDC00..0xE000).contains(next))
        {
            let low = units[index + 1];
            index += 1;
            0x10000 + ((u32::from(unit) - 0xD800) << 10) + (u32::from(low) - 0xDC00)
        } else {
            u32::from(unit)
        };
        index += 1;
        push_code_point(&mut bytes, code_point);
    }
    bytes
}

#[allow(clippy::cast_possible_truncation)]
fn push_code_point(bytes: &mut Vec<u8>, code_point: u32) {
    match code_point {
        0..=0x7F => bytes.push(code_point as u8),
        0x80..=0x7FF => bytes.extend([
            0xC0 | (code_point >> 6) as u8,
            0x80 | (code_point & 0x3F) as u8,
        ]),
        0x800..=0xFFFF => bytes.extend([
            0xE0 | (code_point >> 12) as u8,
            0x80 | ((code_point >> 6) & 0x3F) as u8,
            0x80 | (code_point & 0x3F) as u8,
        ]),
        _ => bytes.extend([
            0xF0 | (code_point >> 18) as u8,
            0x80 | ((code_point >> 12) & 0x3F) as u8,
            0x80 | ((code_point >> 6) & 0x3F) as u8,
            0x80 | (code_point & 0x3F) as u8,
        ]),
    }
}

/// Decodes WTF-8 into UTF-16 code units. Returns `None` for invalid WTF-8, including a
/// surrogate pair encoded as two separate three-byte sequences (which WTF-8 forbids).
#[must_use]
#[allow(clippy::cast_possible_truncation)]
pub fn decode_wtf8(bytes: &[u8]) -> Option<Vec<u16>> {
    let mut units = Vec::with_capacity(bytes.len());
    let mut index = 0;
    let mut previous_lead = false;
    while index < bytes.len() {
        let first = bytes[index];
        let (length, minimum, initial) = match first {
            0x00..=0x7F => (1, 0, u32::from(first)),
            0xC2..=0xDF => (2, 0x80, u32::from(first & 0x1F)),
            0xE0..=0xEF => (3, 0x800, u32::from(first & 0x0F)),
            0xF0..=0xF4 => (4, 0x10000, u32::from(first & 0x07)),
            _ => return None,
        };
        let continuation = bytes.get(index + 1..index + length)?;
        if continuation.len() != length - 1 || continuation.iter().any(|byte| byte & 0xC0 != 0x80) {
            return None;
        }
        let code_point = continuation
            .iter()
            .fold(initial, |value, byte| (value << 6) | u32::from(byte & 0x3F));
        if code_point < minimum || code_point > 0x0010_FFFF {
            return None;
        }
        index += length;
        if code_point >= 0x10000 {
            let value = code_point - 0x10000;
            units.push(0xD800 + (value >> 10) as u16);
            units.push(0xDC00 + (value & 0x3FF) as u16);
            previous_lead = false;
        } else {
            let unit = code_point as u16;
            let is_lead = (0xD800..0xDC00).contains(&unit);
            let is_trail = (0xDC00..0xE000).contains(&unit);
            if previous_lead && is_trail {
                // A paired surrogate must use the four-byte form.
                return None;
            }
            previous_lead = is_lead;
            units.push(unit);
        }
    }
    Some(units)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wtf8_equals_utf8_for_well_formed_text_and_keeps_lone_surrogates() {
        for text in [
            "plain",
            "caf\u{e9}",
            "\u{1f600} emoji",
            "\u{4e2d}\u{6587}",
            "",
        ] {
            let units: Vec<u16> = text.encode_utf16().collect();
            assert_eq!(encode_wtf8(&units), text.as_bytes());
            assert_eq!(decode_wtf8(text.as_bytes()).unwrap(), units);
        }
        let lone = [0x61, 0xD800, 0x62, 0xDC00];
        let encoded = encode_wtf8(&lone);
        assert_eq!(encoded, b"a\xED\xA0\x80b\xED\xB0\x80");
        assert_eq!(decode_wtf8(&encoded).unwrap(), lone);
    }

    #[test]
    fn invalid_wtf8_is_rejected() {
        // Separately encoded surrogate pair, overlong form, stray continuation, truncated.
        for bytes in [
            &b"\xED\xA0\x80\xED\xB0\x80"[..],
            b"\xC0\xAF",
            b"\x80",
            b"\xE2\x82",
            b"\xF5\x80\x80\x80",
        ] {
            assert_eq!(decode_wtf8(bytes), None, "{bytes:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn unix_bytes_round_trip_exactly() {
        let bytes = b"dir/name-\xff\xfe";
        let path = bytes_to_path_buf(bytes).unwrap();
        assert_eq!(path_to_bytes(&path), bytes);
    }
}
