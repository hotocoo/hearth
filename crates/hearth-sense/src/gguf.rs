//! GGUF header reader.
//!
//! Reads only the metadata section, never tensor data, so a 40 GB model is
//! inspected in milliseconds. Every length read from the file is checked
//! against what remains of the file before anything is allocated: GGUF files
//! come from the internet and the parser must not be a way to exhaust memory.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::{Result, SenseError};

const MAGIC: [u8; 4] = *b"GGUF";

/// Longest array kept in memory. Per-layer arrays hold one entry per layer
/// (hundreds); a file may not make a small array of bytes cost a `Value` each.
const MAX_KEPT_ARRAY: u64 = 1 << 20;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Value {
    Uint(u64),
    Int(i64),
    Float(f64),
    Bool(bool),
    String(String),
    /// Numeric array in the model's own `{arch}.` namespace (per-layer
    /// geometry).
    Array(Vec<Value>),
    /// Any other array (tokenizer tables and the like): contents skipped,
    /// length kept. Keeping only model-namespace arrays bounds memory by the
    /// model's layer count instead of by whatever a file claims.
    Skipped {
        len: u64,
    },
}

impl Value {
    #[must_use]
    pub fn as_u64(&self) -> Option<u64> {
        match *self {
            Value::Uint(n) => Some(n),
            Value::Int(n) => u64::try_from(n).ok(),
            Value::Bool(b) => Some(u64::from(b)),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    /// A scalar, or every element of a per-layer array.
    #[must_use]
    pub fn as_u64_list(&self) -> Option<Vec<u64>> {
        match self {
            Value::Array(items) => items.iter().map(Value::as_u64).collect(),
            other => other.as_u64().map(|n| vec![n]),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Header {
    pub version: u32,
    pub tensor_count: u64,
    pub metadata: BTreeMap<String, Value>,
}

impl Header {
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.metadata.get(key)
    }

    #[must_use]
    pub fn architecture(&self) -> Option<&str> {
        self.get("general.architecture").and_then(Value::as_str)
    }

    /// `{arch}.{suffix}`, the naming scheme every model-specific key uses.
    #[must_use]
    pub fn arch_key(&self, suffix: &str) -> Option<&Value> {
        let arch = self.architecture()?;
        self.get(&format!("{arch}.{suffix}"))
    }
}

pub fn read_header(path: &Path) -> Result<Header> {
    let file = File::open(path).map_err(|e| SenseError::Os {
        call: "open gguf",
        detail: format!("{}: {e}", path.display()),
    })?;
    let len = file
        .metadata()
        .map_err(|e| SenseError::Os {
            call: "stat gguf",
            detail: e.to_string(),
        })?
        .len();
    parse(&mut BufReader::new(file), len)
}

/// Parses a header from any reader. `len` bounds every allocation.
pub fn parse<R: Read + Seek>(reader: &mut R, len: u64) -> Result<Header> {
    let mut r = Reader { inner: reader, len };
    let mut magic = [0u8; 4];
    r.exact(&mut magic)?;
    if magic != MAGIC {
        return Err(bad("not a GGUF file (bad magic)"));
    }
    let version = r.u32()?;
    if !(2..=3).contains(&version) {
        // v1 used 32-bit counts; a byte-swapped version means a big-endian
        // file. Neither is produced by current tooling.
        return Err(bad(&format!("unsupported GGUF version {version}")));
    }
    let tensor_count = r.u64()?;
    let kv_count = r.u64()?;
    // Each entry is at least a length-prefixed key and a type tag.
    r.check_count(kv_count, 8 + 4)?;
    let mut metadata = BTreeMap::new();
    let mut arch_prefix: Option<String> = None;
    for _ in 0..kv_count {
        let key = r.string()?;
        let ty = r.u32()?;
        let keep_arrays = arch_prefix.as_deref().is_some_and(|p| key.starts_with(p));
        let value = r.value(ty, keep_arrays)?;
        if key == "general.architecture"
            && let Value::String(arch) = &value
        {
            arch_prefix = Some(format!("{arch}."));
        }
        metadata.insert(key, value);
    }
    Ok(Header {
        version,
        tensor_count,
        metadata,
    })
}

fn bad(detail: &str) -> SenseError {
    SenseError::Parse {
        what: "gguf",
        detail: detail.to_string(),
    }
}

struct Reader<'a, R> {
    inner: &'a mut R,
    len: u64,
}

impl<R: Read + Seek> Reader<'_, R> {
    fn remaining(&mut self) -> Result<u64> {
        let pos = self
            .inner
            .stream_position()
            .map_err(|e| bad(&e.to_string()))?;
        Ok(self.len.saturating_sub(pos))
    }

    /// Rejects a count of items that could not possibly fit in the rest of
    /// the file, before anything is allocated for them.
    fn check_count(&mut self, count: u64, min_item_bytes: u64) -> Result<()> {
        let remaining = self.remaining()?;
        if count.saturating_mul(min_item_bytes) > remaining {
            return Err(bad(&format!(
                "count {count} exceeds the {remaining} bytes left in the file"
            )));
        }
        Ok(())
    }

    fn exact(&mut self, buf: &mut [u8]) -> Result<()> {
        self.inner
            .read_exact(buf)
            .map_err(|e| bad(&format!("truncated: {e}")))
    }

    fn skip(&mut self, bytes: u64) -> Result<()> {
        if bytes > self.remaining()? {
            return Err(bad("truncated while skipping"));
        }
        let offset = i64::try_from(bytes).map_err(|_| bad("skip too large"))?;
        self.inner
            .seek_relative(offset)
            .map_err(|e| bad(&e.to_string()))
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut buf = [0u8; N];
        self.exact(&mut buf)?;
        Ok(buf)
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    fn string(&mut self) -> Result<String> {
        let n = self.u64()?;
        self.check_count(n, 1)?;
        let n = usize::try_from(n).map_err(|_| bad("string too large"))?;
        let mut buf = vec![0u8; n];
        self.exact(&mut buf)?;
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }

    /// Fixed size of a scalar type, or `None` for strings and arrays.
    fn scalar_size(ty: u32) -> Option<u64> {
        match ty {
            0 | 1 | 7 => Some(1),
            2 | 3 => Some(2),
            4..=6 => Some(4),
            10..=12 => Some(8),
            _ => None,
        }
    }

    fn value(&mut self, ty: u32, keep_arrays: bool) -> Result<Value> {
        Ok(match ty {
            0 => Value::Uint(u64::from(self.array::<1>()?[0])),
            1 => Value::Int(i64::from(i8::from_le_bytes(self.array()?))),
            2 => Value::Uint(u64::from(u16::from_le_bytes(self.array()?))),
            3 => Value::Int(i64::from(i16::from_le_bytes(self.array()?))),
            4 => Value::Uint(u64::from(self.u32()?)),
            5 => Value::Int(i64::from(i32::from_le_bytes(self.array()?))),
            6 => Value::Float(f64::from(f32::from_le_bytes(self.array()?))),
            7 => Value::Bool(self.array::<1>()?[0] != 0),
            8 => Value::String(self.string()?),
            9 => {
                let elem = self.u32()?;
                let n = self.u64()?;
                match Self::scalar_size(elem) {
                    Some(size) if keep_arrays => {
                        if n > MAX_KEPT_ARRAY {
                            return Err(bad(&format!(
                                "array of {n} entries is more than the {MAX_KEPT_ARRAY} kept"
                            )));
                        }
                        self.check_count(n, size)?;
                        Value::Array(
                            (0..n)
                                .map(|_| self.value(elem, false))
                                .collect::<Result<_>>()?,
                        )
                    }
                    Some(size) => {
                        self.skip(n.saturating_mul(size))?;
                        Value::Skipped { len: n }
                    }
                    None if elem == 8 => {
                        self.check_count(n, 8)?;
                        for _ in 0..n {
                            let len = self.u64()?;
                            self.skip(len)?;
                        }
                        Value::Skipped { len: n }
                    }
                    None => return Err(bad(&format!("unsupported array element type {elem}"))),
                }
            }
            10 => Value::Uint(self.u64()?),
            11 => Value::Int(i64::from_le_bytes(self.array()?)),
            12 => Value::Float(f64::from_le_bytes(self.array()?)),
            other => return Err(bad(&format!("unknown value type {other}"))),
        })
    }
}

/// All files of a model: the file itself, or every shard of a split model
/// (`name-00001-of-00003.gguf`), located next to the given one.
#[must_use]
pub fn model_files(path: &Path) -> Vec<PathBuf> {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return vec![path.to_path_buf()];
    };
    let Some((prefix, total)) = split_name(name) else {
        return vec![path.to_path_buf()];
    };
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    (1..=total)
        .map(|i| dir.join(format!("{prefix}-{i:05}-of-{total:05}.gguf")))
        .collect()
}

/// `("name", 3)` for `name-00002-of-00003.gguf`.
fn split_name(name: &str) -> Option<(&str, u32)> {
    let stem = name.strip_suffix(".gguf")?;
    let (head, total) = stem.rsplit_once("-of-")?;
    let (prefix, index) = head.rsplit_once('-')?;
    if index.len() != 5 || total.len() != 5 {
        return None;
    }
    index.parse::<u32>().ok()?;
    // A total of 00000 names no shards.
    Some((prefix, total.parse().ok().filter(|&t| t > 0)?))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Cursor;

    /// Builds a GGUF v3 header from (key, type, encoded value) entries.
    pub(crate) fn build(entries: &[(&str, u32, Vec<u8>)]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"GGUF");
        out.extend_from_slice(&3u32.to_le_bytes());
        out.extend_from_slice(&0u64.to_le_bytes());
        out.extend_from_slice(&(entries.len() as u64).to_le_bytes());
        for (key, ty, value) in entries {
            out.extend_from_slice(&(key.len() as u64).to_le_bytes());
            out.extend_from_slice(key.as_bytes());
            out.extend_from_slice(&ty.to_le_bytes());
            out.extend_from_slice(value);
        }
        out
    }

    pub(crate) fn s(text: &str) -> Vec<u8> {
        let mut v = (text.len() as u64).to_le_bytes().to_vec();
        v.extend_from_slice(text.as_bytes());
        v
    }

    pub(crate) fn u32v(n: u32) -> Vec<u8> {
        n.to_le_bytes().to_vec()
    }

    pub(crate) fn u32_array(items: &[u32]) -> Vec<u8> {
        let mut v = 4u32.to_le_bytes().to_vec();
        v.extend_from_slice(&(items.len() as u64).to_le_bytes());
        for i in items {
            v.extend_from_slice(&i.to_le_bytes());
        }
        v
    }

    fn parse_bytes(bytes: &[u8]) -> Result<Header> {
        parse(&mut Cursor::new(bytes), bytes.len() as u64)
    }

    #[test]
    fn reads_scalars_strings_and_arrays() {
        let mut tokens = 8u32.to_le_bytes().to_vec();
        tokens.extend_from_slice(&2u64.to_le_bytes());
        tokens.extend(s("a"));
        tokens.extend(s("bc"));
        let bytes = build(&[
            ("general.architecture", 8, s("llama")),
            ("llama.block_count", 4, u32v(32)),
            ("llama.attention.head_count_kv", 9, u32_array(&[8, 8, 0])),
            ("tokenizer.ggml.tokens", 9, tokens),
            ("llama.rope.freq_base", 6, 10000f32.to_le_bytes().to_vec()),
        ]);
        let h = parse_bytes(&bytes).unwrap();
        assert_eq!(h.architecture(), Some("llama"));
        assert_eq!(h.arch_key("block_count").and_then(Value::as_u64), Some(32));
        assert_eq!(
            h.arch_key("attention.head_count_kv")
                .and_then(Value::as_u64_list),
            Some(vec![8, 8, 0])
        );
        assert_eq!(
            h.get("tokenizer.ggml.tokens"),
            Some(&Value::Skipped { len: 2 })
        );
        assert_eq!(h.arch_key("rope.freq_base"), Some(&Value::Float(10000.0)));
    }

    #[test]
    fn rejects_bad_magic_and_versions() {
        assert!(parse_bytes(b"GGML\x03\0\0\0").is_err());
        let mut v1 = build(&[]);
        v1[4] = 1;
        assert!(parse_bytes(&v1).is_err());
    }

    #[test]
    fn hostile_lengths_fail_without_allocating() {
        // A string claiming u64::MAX bytes in a tiny file.
        let mut bytes = build(&[]);
        bytes[16..24].copy_from_slice(&1u64.to_le_bytes());
        bytes.extend_from_slice(&u64::MAX.to_le_bytes());
        assert!(parse_bytes(&bytes).is_err());
        // A kv count far beyond what the file could hold.
        let mut bytes = build(&[]);
        bytes[16..24].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(parse_bytes(&bytes).is_err());
        // An array of huge length with a small element type.
        let mut arr = 4u32.to_le_bytes().to_vec();
        arr.extend_from_slice(&(u64::MAX / 2).to_le_bytes());
        assert!(parse_bytes(&build(&[("x", 9, arr)])).is_err());
    }

    #[test]
    fn truncated_input_is_an_error_not_a_panic() {
        let bytes = build(&[("general.architecture", 8, s("llama"))]);
        for cut in 0..bytes.len() {
            let _ = parse_bytes(&bytes[..cut]);
        }
    }

    #[test]
    fn split_model_shards_are_enumerated() {
        let files = model_files(Path::new("/m/qwen-00002-of-00003.gguf"));
        assert_eq!(
            files,
            vec![
                PathBuf::from("/m/qwen-00001-of-00003.gguf"),
                PathBuf::from("/m/qwen-00002-of-00003.gguf"),
                PathBuf::from("/m/qwen-00003-of-00003.gguf"),
            ]
        );
        assert_eq!(
            model_files(Path::new("/m/plain.gguf")),
            vec![PathBuf::from("/m/plain.gguf")]
        );
        // A shard total of zero names no files: the path stands alone.
        let odd = Path::new("/m/q-00001-of-00000.gguf");
        assert_eq!(model_files(odd), vec![odd.to_path_buf()]);
    }
}
