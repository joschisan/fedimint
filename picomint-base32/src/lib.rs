use std::collections::BTreeMap;

use anyhow::{Context, ensure};
use picomint_encoding::{Decodable, Encodable};

/// Lowercase RFC 4648 Base32hex alphabet (32 characters).
const RFC4648: [u8; 32] = *b"0123456789abcdefghijklmnopqrstuv";

/// Prefix tagging user-facing Picomint base32 strings.
const PREFIX: &str = "picomint";

/// Encodes `value` as base32hex, prefixed with `picomint`.
pub fn encode<T: Encodable>(value: &T) -> String {
    format!("{PREFIX}{}", encode_bytes(&value.consensus_encode_to_vec()))
}

/// Decodes a `picomint`-prefixed base32hex string back into `T`.
pub fn decode<T: Decodable>(s: &str) -> anyhow::Result<T> {
    let s = s.to_lowercase();

    ensure!(s.starts_with(PREFIX), "Invalid prefix");

    let bytes = decode_bytes(&s[PREFIX.len()..])?;

    Ok(T::consensus_decode_exact(&bytes)?)
}

fn encode_bytes(input: &[u8]) -> String {
    let mut output = Vec::with_capacity(((8 * input.len()) / 5) + 1);

    let mut buffer = 0;
    let mut bits = 0;

    for byte in input {
        buffer |= (*byte as usize) << bits;
        bits += 8;

        while bits >= 5 {
            output.push(RFC4648[buffer & 0b11111]);

            buffer >>= 5;
            bits -= 5;
        }
    }

    if bits > 0 {
        output.push(RFC4648[buffer & 0b11111]);
    }

    String::from_utf8(output).expect("RFC4648 alphabet is ASCII")
}

fn decode_bytes(input: &str) -> anyhow::Result<Vec<u8>> {
    let decode_table = RFC4648
        .iter()
        .enumerate()
        .map(|(i, c)| (*c, i))
        .collect::<BTreeMap<u8, usize>>();

    let mut output = Vec::with_capacity(((5 * input.len()) / 8) + 1);

    let mut buffer = 0;
    let mut bits = 0;

    for byte in input.as_bytes() {
        let value = decode_table
            .get(byte)
            .copied()
            .context("Invalid character encountered")?;

        buffer |= value << bits;
        bits += 5;

        while bits >= 8 {
            output.push((buffer & 0xFF) as u8);

            buffer >>= 8;
            bits -= 8;
        }
    }

    Ok(output)
}

#[test]
fn test_roundtrip() {
    let data: Vec<u8> = vec![0x50, 0xAB, 0x3F, 0x77, 0x01, 0xCD, 0x55, 0xFE, 0x10, 0x99];

    let encoded = encode(&data);
    assert!(encoded.starts_with(PREFIX));

    let decoded: Vec<u8> = decode(&encoded).unwrap();
    assert_eq!(decoded, data);

    let decoded: Vec<u8> = decode(&encoded.to_ascii_uppercase()).unwrap();
    assert_eq!(decoded, data);
}
