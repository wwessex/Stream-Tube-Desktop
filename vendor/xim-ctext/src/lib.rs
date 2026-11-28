//! Currently only support utf8 mode

#![no_std]

extern crate alloc;

#[cfg(feature = "std")]
extern crate std;

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

#[cfg(feature = "std")]
use std::io::{self, Write};

const UTF8_START: &[u8] = &[0x1B, 0x25, 0x47];
const UTF8_END: &[u8] = &[0x1B, 0x25, 0x40];

/// Wrapper for reduce allocation
#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct CText<'s> {
    utf8: &'s str,
}

impl<'s> fmt::Debug for CText<'s> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.utf8)
    }
}

impl<'s> fmt::Display for CText<'s> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.utf8)
    }
}

impl<'s> CText<'s> {
    pub const fn new(utf8: &'s str) -> Self {
        Self { utf8 }
    }

    pub const fn len(self) -> usize {
        self.utf8.len() + UTF8_START.len() + UTF8_END.len()
    }

    #[cfg(feature = "std")]
    pub fn write(self, mut out: impl Write) -> io::Result<usize> {
        let mut writed = 0;
        writed += out.write(UTF8_START)?;
        writed += out.write(self.utf8.as_bytes())?;
        writed += out.write(UTF8_END)?;
        Ok(writed)
    }
}

/// Encoding utf8 to COMPOUND_TEXT with utf8 escape
pub fn utf8_to_compound_text(text: &str) -> Vec<u8> {
    let mut ret = Vec::with_capacity(text.len() + 6);
    ret.extend_from_slice(UTF8_START);
    ret.extend_from_slice(text.as_bytes());
    ret.extend_from_slice(UTF8_END);
    ret
}

#[derive(Debug, Clone)]
pub enum DecodeError {
    InvalidEncoding,
    UnsupportedEncoding,
    Utf8Error(alloc::string::FromUtf8Error),
}

impl From<alloc::string::FromUtf8Error> for DecodeError {
    fn from(err: alloc::string::FromUtf8Error) -> Self {
        DecodeError::Utf8Error(err)
    }
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidEncoding => write!(f, "Invalid compound text"),
            Self::UnsupportedEncoding => write!(f, "This encoding is not supported yet"),
            Self::Utf8Error(e) => write!(f, "Not a valid utf8 {}", e),
        }
    }
}

/// A whole run of bytes through `encoding`.
fn decode_into(out: &mut String, encoding: &'static encoding_rs::Encoding, bytes: &[u8]) {
    let mut decoder = encoding.new_decoder_without_bom_handling();
    out.reserve(decoder.max_utf8_buffer_length(bytes.len()).unwrap_or(bytes.len() * 3));
    loop {
        let (result, read, _) = decoder.decode_to_string(&bytes[..], out, true);
        match result {
            encoding_rs::CoderResult::InputEmpty => break,
            encoding_rs::CoderResult::OutputFull => {
                out.reserve(decoder.max_utf8_buffer_length(bytes.len() - read).unwrap_or(16));
                return decode_into(out, encoding, &bytes[read..]);
            }
        }
    }
}

/// The 96-character set an `ESC - F` sequence puts in GR (ISO 8859 right halves).
fn right_half(f: u8) -> Option<&'static encoding_rs::Encoding> {
    Some(match f {
        b'A' => encoding_rs::WINDOWS_1252, // ISO 8859-1
        b'B' => encoding_rs::ISO_8859_2,
        b'C' => encoding_rs::ISO_8859_3,
        b'D' => encoding_rs::ISO_8859_4,
        b'F' => encoding_rs::ISO_8859_7,
        b'G' => encoding_rs::ISO_8859_6,
        b'H' => encoding_rs::ISO_8859_8,
        b'L' => encoding_rs::ISO_8859_5,
        b'M' => encoding_rs::WINDOWS_1254, // ISO 8859-9
        b'V' => encoding_rs::ISO_8859_10,
        b'Y' => encoding_rs::ISO_8859_13,
        b'_' => encoding_rs::ISO_8859_14,
        b'b' => encoding_rs::ISO_8859_15,
        b'f' => encoding_rs::ISO_8859_16,
        _ => return None,
    })
}

/// Decodes COMPOUND_TEXT: UTF-8 segments, the ISO 8859 sets in GR (Latin-1
/// by default), ASCII in GL and JIS X 0208. Unknown sets become U+FFFD instead
/// of an error, so an input method never brings the client down.
pub fn compound_text_to_utf8(bytes: &[u8]) -> Result<String, DecodeError> {
    #[derive(Clone, Copy)]
    enum Gl {
        Ascii,
        Jis,
        Unknown,
    }
    let mut out = String::new();
    let mut gl = Gl::Ascii;
    let mut gr: Option<&'static encoding_rs::Encoding> = Some(encoding_rs::WINDOWS_1252);
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1B {
            let rest = &bytes[i + 1..];
            match rest {
                // UTF-8 up to ESC % @ (or the end)
                [0x25, 0x47, ..] => {
                    let body = &rest[2..];
                    let end = body.windows(3).position(|w| w == UTF8_END).unwrap_or(body.len());
                    out.push_str(&String::from_utf8_lossy(&body[..end]));
                    i += 3 + (end + 3).min(body.len());
                }
                // 94-character set to GL
                [0x28, f, ..] => {
                    gl = if *f == b'B' || *f == b'J' { Gl::Ascii } else { Gl::Unknown };
                    i += 3;
                }
                // 96-character set to GR
                [0x2D, f, ..] => {
                    gr = right_half(*f);
                    i += 3;
                }
                // 94^2-character set to GL or GR
                [0x24, 0x28, f, ..] | [0x24, 0x29, f, ..] => {
                    gl = if *f == b'B' { Gl::Jis } else { Gl::Unknown };
                    i += 4;
                }
                _ => return Err(DecodeError::InvalidEncoding),
            }
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i] != 0x1B {
            i += 1;
        }
        let run = &bytes[start..i];
        match gl {
            Gl::Jis => {
                // JIS X 0208 in GL: two bytes per character
                let mut jis = Vec::with_capacity(run.len() + 3);
                jis.extend_from_slice(&[0x1B, 0x24, 0x42]);
                jis.extend_from_slice(run);
                decode_into(&mut out, encoding_rs::ISO_2022_JP, &jis);
            }
            Gl::Ascii | Gl::Unknown => {
                for &b in run {
                    if b < 0x80 {
                        match gl {
                            Gl::Unknown if b > 0x20 && b < 0x7F => out.push('\u{FFFD}'),
                            _ => out.push(b as char),
                        }
                    } else if b >= 0xA0 {
                        match gr {
                            Some(encoding) => decode_into(&mut out, encoding, &[b]),
                            None => out.push('\u{FFFD}'),
                        }
                    }
                    // C1 controls (0x80..0xA0) carry no text
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use alloc::string::ToString;

    #[test]
    fn korean() {
        const UTF8: &str = "가나다";
        const COMP: &[u8] = &[
            27, 37, 71, 234, 176, 128, 235, 130, 152, 235, 139, 164, 27, 37, 64,
        ];
        assert_eq!(crate::utf8_to_compound_text(UTF8), COMP);
        assert_eq!(crate::compound_text_to_utf8(COMP).unwrap(), UTF8);
    }

    #[test]
    fn iso_2011_jp() {
        const UTF8: &str = "東京";
        const COMP: &[u8] = &[27, 36, 40, 66, 69, 108, 53, 126];
        assert_eq!(crate::compound_text_to_utf8(COMP).unwrap(), UTF8);
    }

    #[test]
    fn cyrillic_in_iso_8859_5() {
        // ESC - L, then "Привет" in ISO 8859-5 (issue #248)
        const COMP: &[u8] = &[27, 0x2D, b'L', 0xBF, 0xE0, 0xD8, 0xD2, 0xD5, 0xE2];
        assert_eq!(crate::compound_text_to_utf8(COMP).unwrap(), "Привет");
    }

    #[test]
    fn ascii_latin1_and_mixed_sets() {
        assert_eq!(crate::compound_text_to_utf8(b"hello").unwrap(), "hello");
        // Latin-1 is in GR without any escape
        assert_eq!(crate::compound_text_to_utf8(&[b'c', b'a', b'f', 0xE9]).unwrap(), "café");
        // Greek, then UTF-8, then back to Latin-1
        let comp = [27, 0x2D, b'F', 0xE1, 27, 0x25, 0x47, 0xE2, 0x82, 0xAC, 27, 0x25, 0x40, 27, 0x2D, b'A', 0xFC];
        assert_eq!(crate::compound_text_to_utf8(&comp).unwrap(), "α€ü");
        assert_eq!(crate::compound_text_to_utf8(&[]).unwrap(), "");
    }

    #[test]
    fn unknown_sets_never_fail() {
        assert_eq!(crate::compound_text_to_utf8(&[27, 0x2D, b'~', 0xC0]).unwrap(), "\u{FFFD}");
        assert_eq!(crate::compound_text_to_utf8(&[27, 0x24, 0x28, b'A', 0x30, 0x21]).unwrap(), "\u{FFFD}\u{FFFD}");
        // an unterminated UTF-8 segment keeps what it has
        assert_eq!(crate::compound_text_to_utf8(&[27, 0x25, 0x47, b'o', b'k']).unwrap(), "ok".to_string());
    }
}
