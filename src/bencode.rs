use std::{
    collections::BTreeMap,
    io::{self, Write},
    ops::Range,
};

use anyhow::{Result, bail, ensure};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Int(i64),
    Bytes(Vec<u8>),
    List(Vec<Value>),
    Dict(BTreeMap<Vec<u8>, Value>),
}

impl Value {
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Self::Int(v) => Some(*v),
            _ => None,
        }
    }
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Bytes(v) => Some(v),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        std::str::from_utf8(self.as_bytes()?).ok()
    }
    pub fn as_list(&self) -> Option<&[Value]> {
        match self {
            Self::List(v) => Some(v),
            _ => None,
        }
    }
    pub fn as_dict(&self) -> Option<&BTreeMap<Vec<u8>, Value>> {
        match self {
            Self::Dict(v) => Some(v),
            _ => None,
        }
    }
    pub fn as_dict_mut(&mut self) -> Option<&mut BTreeMap<Vec<u8>, Value>> {
        match self {
            Self::Dict(v) => Some(v),
            _ => None,
        }
    }
    pub fn get(&self, key: &[u8]) -> Option<&Value> {
        self.as_dict()?.get(key)
    }
    pub fn get_mut(&mut self, key: &[u8]) -> Option<&mut Value> {
        self.as_dict_mut()?.get_mut(key)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_bytes: usize,
    pub max_depth: usize,
    pub max_values: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_bytes: 256 * 1024 * 1024,
            max_depth: 128,
            max_values: 1_000_000,
        }
    }
}

pub fn parse(input: &[u8]) -> Result<Value> {
    Ok(parse_with_info_range(input)?.0)
}

pub fn parse_with_info_range(input: &[u8]) -> Result<(Value, Option<Range<usize>>)> {
    parse_with_limits(input, Limits::default())
}

pub fn parse_with_limits(input: &[u8], limits: Limits) -> Result<(Value, Option<Range<usize>>)> {
    ensure!(
        input.len() <= limits.max_bytes,
        "bencode input exceeds {} byte limit",
        limits.max_bytes
    );
    let mut parser = Parser {
        input,
        pos: 0,
        values: 0,
        limits,
        info_range: None,
    };
    let value = parser.value(0)?;
    ensure!(
        parser.pos == input.len(),
        "trailing bencode data at byte {}",
        parser.pos
    );
    Ok((value, parser.info_range))
}

struct Parser<'a> {
    input: &'a [u8],
    pos: usize,
    values: usize,
    limits: Limits,
    info_range: Option<Range<usize>>,
}

impl<'a> Parser<'a> {
    fn value(&mut self, depth: usize) -> Result<Value> {
        ensure!(
            depth <= self.limits.max_depth,
            "bencode nesting exceeds depth limit at byte {}",
            self.pos
        );
        ensure!(
            self.values < self.limits.max_values,
            "bencode value count exceeds limit at byte {}",
            self.pos
        );
        self.values += 1;
        match self.input.get(self.pos).copied() {
            Some(b'i') => self.integer(),
            Some(b'0'..=b'9') => Ok(Value::Bytes(self.bytes()?.to_vec())),
            Some(b'l') => {
                self.pos += 1;
                let mut values = Vec::new();
                while self.input.get(self.pos) != Some(&b'e') {
                    values.push(self.value(depth + 1)?);
                }
                self.pos += 1;
                Ok(Value::List(values))
            }
            Some(b'd') => {
                self.pos += 1;
                let mut values: BTreeMap<Vec<u8>, Value> = BTreeMap::new();
                while self.input.get(self.pos) != Some(&b'e') {
                    let key = self.bytes()?;
                    if let Some((last, _)) = values.last_key_value() {
                        ensure!(
                            last.as_slice() < key,
                            "dictionary keys are duplicated or unsorted at byte {}",
                            self.pos
                        );
                    }
                    let start = self.pos;
                    let value = self.value(depth + 1)?;
                    if depth == 0 && key == b"info" {
                        self.info_range = Some(start..self.pos);
                    }
                    values.insert(key.to_vec(), value);
                }
                self.pos += 1;
                Ok(Value::Dict(values))
            }
            Some(byte) => bail!("invalid bencode marker 0x{byte:02x} at byte {}", self.pos),
            None => bail!("unexpected end of bencode at byte {}", self.pos),
        }
    }

    fn bytes(&mut self) -> Result<&'a [u8]> {
        let start = self.pos;
        let mut length = 0usize;
        while let Some(byte @ b'0'..=b'9') = self.input.get(self.pos) {
            length = length
                .checked_mul(10)
                .and_then(|n| n.checked_add((byte - b'0') as usize))
                .ok_or_else(|| anyhow::anyhow!("byte string length overflow at byte {start}"))?;
            self.pos += 1;
        }
        ensure!(
            self.pos > start && self.input.get(self.pos) == Some(&b':'),
            "invalid byte string length at byte {start}"
        );
        ensure!(
            self.pos - start == 1 || self.input[start] != b'0',
            "leading zero in byte string length at byte {start}"
        );
        self.pos += 1;
        let end = self
            .pos
            .checked_add(length)
            .filter(|end| *end <= self.input.len())
            .ok_or_else(|| {
                anyhow::anyhow!("truncated byte string at byte {start}: expected {length} bytes")
            })?;
        let value = &self.input[self.pos..end];
        self.pos = end;
        Ok(value)
    }

    fn integer(&mut self) -> Result<Value> {
        self.pos += 1;
        let start = self.pos;
        if self.input.get(self.pos) == Some(&b'-') {
            self.pos += 1;
        }
        let digits = self.pos;
        while self.input.get(self.pos).is_some_and(u8::is_ascii_digit) {
            self.pos += 1;
        }
        ensure!(
            self.pos > digits && self.input.get(self.pos) == Some(&b'e'),
            "invalid integer at byte {start}"
        );
        ensure!(
            self.pos - digits == 1 || self.input[digits] != b'0',
            "leading zero in integer at byte {start}"
        );
        ensure!(
            digits == start || self.input[digits] != b'0',
            "negative zero at byte {start}"
        );
        let value = std::str::from_utf8(&self.input[start..self.pos])?
            .parse::<i64>()
            .map_err(|_| anyhow::anyhow!("integer outside signed 64-bit range at byte {start}"))?;
        self.pos += 1;
        Ok(Value::Int(value))
    }
}

pub fn encode(value: &Value) -> Vec<u8> {
    let mut output = Vec::new();
    encode_to_writer(value, &mut output).expect("writing to a byte vector cannot fail");
    output
}

pub fn encode_to_writer(value: &Value, output: &mut impl Write) -> io::Result<()> {
    match value {
        Value::Int(value) => write!(output, "i{value}e"),
        Value::Bytes(value) => write_bytes(value, output),
        Value::List(values) => {
            output.write_all(b"l")?;
            for value in values {
                encode_to_writer(value, output)?;
            }
            output.write_all(b"e")
        }
        Value::Dict(values) => {
            output.write_all(b"d")?;
            for (key, value) in values {
                write_bytes(key, output)?;
                encode_to_writer(value, output)?;
            }
            output.write_all(b"e")
        }
    }
}

fn write_bytes(value: &[u8], output: &mut impl Write) -> io::Result<()> {
    write!(output, "{}:", value.len())?;
    output.write_all(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_binary_and_unknown_fields() {
        let input =
            b"d4:infod4:name3:a\x00\xffe1:\xffl0:i-9223372036854775808ei9223372036854775807eee";
        let (value, range) = parse_with_info_range(input).unwrap();
        assert_eq!(encode(&value), input);
        assert_eq!(&input[range.unwrap()], b"d4:name3:a\x00\xffe");
        assert!(value.get(b"\xff").is_some());
    }

    #[test]
    fn only_captures_top_level_info() {
        let (_, range) = parse_with_info_range(b"d1:ad4:infoi1eee").unwrap();
        assert_eq!(range, None);
    }

    #[test]
    fn rejects_malformed_inputs() {
        let cases: &[&[u8]] = &[
            b"",
            b"e",
            b"i",
            b"ie",
            b"i-e",
            b"i+1e",
            b"i01e",
            b"i-0e",
            b"i-01e",
            b"i9223372036854775808e",
            b"i-9223372036854775809e",
            b"i1xe",
            b"i1eX",
            b"01:a",
            b"-1:a",
            b"1",
            b"1:",
            b"2:a",
            b"9999999999999999999999999:a",
            b"li1e",
            b"d",
            b"d1:ae",
            b"di1ei2ee",
            b"d1:bi1e1:ai2ee",
            b"d1:ai1e1:ai2ee",
        ];
        for input in cases {
            assert!(parse(input).is_err(), "accepted {input:?}");
        }
    }

    #[test]
    fn enforces_resource_limits() {
        assert!(
            parse_with_limits(
                b"0:",
                Limits {
                    max_bytes: 1,
                    ..Limits::default()
                }
            )
            .is_err()
        );
        assert!(
            parse_with_limits(
                b"lli1eee",
                Limits {
                    max_depth: 1,
                    ..Limits::default()
                }
            )
            .is_err()
        );
        assert!(
            parse_with_limits(
                b"li1ei2ee",
                Limits {
                    max_values: 2,
                    ..Limits::default()
                }
            )
            .is_err()
        );
        let mut deep = vec![b'l'; 1000];
        deep.extend(vec![b'e'; 1000]);
        assert!(parse(&deep).is_err());
    }

    #[test]
    fn encoder_sorts_raw_keys() {
        let value = Value::Dict(BTreeMap::from([
            (vec![255], Value::Int(3)),
            (b"z".to_vec(), Value::Int(2)),
            (vec![0], Value::Int(1)),
        ]));
        assert_eq!(encode(&value), b"d1:\x00i1e1:zi2e1:\xffi3ee");
    }

    #[test]
    fn reports_writer_failure() {
        let mut buffer = [0u8; 1];
        assert!(encode_to_writer(&Value::Bytes(vec![1, 2, 3]), &mut &mut buffer[..]).is_err());
    }
}
