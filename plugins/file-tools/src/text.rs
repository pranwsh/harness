use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

pub const MAX_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Clone, Copy)]
enum Encoding {
    Utf8,
    Utf8Bom,
    Utf16Le,
    Utf16Be,
}

#[derive(Clone)]
pub(crate) struct Document {
    pub text: String,
    encoding: Encoding,
    pub newline: &'static str,
}

pub(crate) fn version(bytes: &[u8]) -> String {
    format!("{:016x}", xxhash_rust::xxh3::xxh3_64(bytes))
}

#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Anchor {
    pub line: usize,
    pub hash: String,
}

pub(crate) fn read_bytes(path: &Path) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let file = fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !file.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(format!("{} exceeds {MAX_BYTES} bytes", path.display()));
    }
    Ok(bytes)
}

impl Document {
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let (encoding, body) = if bytes.starts_with(&[0xff, 0xfe]) {
            (Encoding::Utf16Le, &bytes[2..])
        } else if bytes.starts_with(&[0xfe, 0xff]) {
            (Encoding::Utf16Be, &bytes[2..])
        } else if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
            (Encoding::Utf8Bom, &bytes[3..])
        } else {
            (Encoding::Utf8, bytes)
        };
        let text = match encoding {
            Encoding::Utf8 | Encoding::Utf8Bom => String::from_utf8(body.to_vec())
                .map_err(|_| "unsupported encoding: expected UTF-8 or BOM-marked UTF-16")?,
            Encoding::Utf16Le | Encoding::Utf16Be => {
                if body.len() % 2 != 0 {
                    return Err("invalid UTF-16 byte count".into());
                }
                let units = body
                    .chunks_exact(2)
                    .map(|b| {
                        if matches!(encoding, Encoding::Utf16Le) {
                            u16::from_le_bytes([b[0], b[1]])
                        } else {
                            u16::from_be_bytes([b[0], b[1]])
                        }
                    })
                    .collect::<Vec<_>>();
                String::from_utf16(&units).map_err(|_| "invalid UTF-16")?
            }
        };
        if text.contains('\0') {
            return Err("binary text (NUL) is not supported".into());
        }
        let mut crlf = false;
        let mut bare_lf = false;
        let raw = text.as_bytes();
        for (i, b) in raw.iter().enumerate() {
            if *b == b'\r' {
                if raw.get(i + 1) != Some(&b'\n') {
                    return Err("CR-only line endings are not supported; no bytes changed".into());
                }
                crlf = true;
            }
            if *b == b'\n' && (i == 0 || raw[i - 1] != b'\r') {
                bare_lf = true;
            }
        }
        if crlf && bare_lf {
            return Err("mixed line endings are not supported; no bytes changed".into());
        }
        Ok(Self {
            text: if crlf {
                text.replace("\r\n", "\n")
            } else {
                text
            },
            encoding,
            newline: if crlf { "\r\n" } else { "\n" },
        })
    }

    pub fn new(text: String) -> Result<Self, String> {
        Self::decode(text.as_bytes())
    }

    pub fn encode(&self, text: &str) -> Vec<u8> {
        let text = text.replace('\n', self.newline);
        match self.encoding {
            Encoding::Utf8 => text.into_bytes(),
            Encoding::Utf8Bom => [vec![0xef, 0xbb, 0xbf], text.into_bytes()].concat(),
            Encoding::Utf16Le | Encoding::Utf16Be => {
                let le = matches!(self.encoding, Encoding::Utf16Le);
                let mut out = if le {
                    vec![0xff, 0xfe]
                } else {
                    vec![0xfe, 0xff]
                };
                for u in text.encode_utf16() {
                    out.extend(if le { u.to_le_bytes() } else { u.to_be_bytes() });
                }
                out
            }
        }
    }

    pub fn ranges(&self) -> Vec<std::ops::Range<usize>> {
        let mut start = 0;
        self.text
            .split_inclusive('\n')
            .map(|line| {
                let range = start..start + line.len();
                start = range.end;
                range
            })
            .collect()
    }

    pub fn anchor(&self, line: usize) -> Option<Anchor> {
        self.text
            .split_terminator('\n')
            .nth(line.checked_sub(1)?)
            .map(|text| Anchor {
                line,
                hash: version(text.as_bytes())[..12].to_owned(),
            })
    }

    pub fn rows(
        &self,
        lines: impl IntoIterator<Item = usize>,
        anchors: bool,
        width: usize,
    ) -> String {
        // Stream through lines once; a small window never allocates a pointer
        // table or hashes for every line in the file.
        let mut source = self.text.split_terminator('\n').enumerate().peekable();
        let mut out = String::new();
        for n in lines {
            while source.peek().is_some_and(|(i, _)| i + 1 < n) {
                source.next();
            }
            if let Some((i, text)) = source.peek().copied().filter(|(i, _)| i + 1 == n) {
                let _ = i;
                let clipped: String = text.chars().take(width).collect();
                let overflow = if clipped.len() < text.len() {
                    " … [line truncated; anchor covers full line]"
                } else {
                    ""
                };
                let prefix = if anchors {
                    format!("{n}:{}|", version(text.as_bytes())[..12].to_owned())
                } else {
                    format!("{n}: ")
                };
                out.push_str(&format!("{prefix}{clipped}{overflow}\n"));
                if out.len() > 1024 * 1024 {
                    out.push_str("[overflow: rendered region exceeds 1 MiB; narrow selection]\n");
                    break;
                }
            }
        }
        out
    }
}

pub(crate) fn bounded(text: String, max_tokens: usize) -> Result<String, String> {
    if !(64..=32768).contains(&max_tokens) {
        return Err("max_tokens must be between 64 and 32768".into());
    }
    let tokenizer = tiktoken_rs::cl100k_base_singleton();
    if tokenizer.encode_ordinary(&text).len() <= max_tokens {
        return Ok(text);
    }
    let note = "\n[overflow: response truncated; narrow selection or raise max_tokens; tokenizer=cl100k_base]\n";
    let mut ends = text.char_indices().map(|(i, _)| i).collect::<Vec<_>>();
    ends.push(text.len());
    let mut lo = 0;
    let mut hi = ends.len() - 1;
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if tokenizer
            .encode_ordinary(&format!("{}{note}", &text[..ends[mid]]))
            .len()
            <= max_tokens
        {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    // Avoid returning a partial anchored row as if it were complete.
    let prefix = &text[..ends[lo]];
    let end = prefix.rfind('\n').map_or(0, |n| n + 1);
    let mut out = format!("{}{note}", &prefix[..end]);
    // BPE boundaries can change when the overflow marker is appended. Verify
    // the actual final response, not only the pre-newline candidate.
    while tokenizer.encode_ordinary(&out).len() > max_tokens {
        let prefix = &out[..out.len() - note.len()];
        let shortened = prefix
            .trim_end_matches('\n')
            .rfind('\n')
            .map_or(0, |n| n + 1);
        out = format!("{}{note}", &prefix[..shortened]);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn conventions_roundtrip() {
        for bytes in [
            b"a\r\nb\r\n".to_vec(),
            vec![255, 254, 97, 0, 10, 0],
            b"\xef\xbb\xbfa\nb".to_vec(),
        ] {
            let d = Document::decode(&bytes).unwrap();
            assert_eq!(d.encode(&d.text), bytes);
        }
        assert!(Document::decode(b"a\r\nb\n").is_err());
        assert!(Document::decode(&[0xff]).is_err());
    }
    #[test]
    fn bounded_output() {
        let out = bounded("hello world\n".repeat(1000), 64).unwrap();
        assert!(out.contains("overflow"));
        assert!(
            tiktoken_rs::cl100k_base_singleton()
                .encode_ordinary(&out)
                .len()
                <= 64
        );
    }
}
