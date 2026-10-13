use std::collections::BTreeMap;

use rustide::{
    bencode::{self, Limits, Value},
    metadata,
};

fn dictionary(entries: &[(&[u8], Value)]) -> Value {
    Value::Dict(
        entries
            .iter()
            .map(|(k, v)| (k.to_vec(), v.clone()))
            .collect(),
    )
}

fn corpus() -> Vec<Vec<u8>> {
    let info = dictionary(&[
        (b"length", Value::Int(1)),
        (b"name", Value::Bytes(b"x".to_vec())),
        (b"piece length", Value::Int(16384)),
        (b"pieces", Value::Bytes(vec![0; 20])),
    ]);
    let mut seeds = vec![bencode::encode(&dictionary(&[(b"info", info.clone())]))];
    for number in [i64::MIN, -1, 0, 1, 16384, i64::MAX] {
        for field in [b"length".as_slice(), b"piece length"] {
            let mut changed = info.clone();
            changed
                .as_dict_mut()
                .unwrap()
                .insert(field.to_vec(), Value::Int(number));
            seeds.push(bencode::encode(&dictionary(&[(b"info", changed)])));
        }
    }
    for name in [
        b"..".as_slice(),
        b"a/b",
        b"a\\b",
        b"C:x",
        b"a\0b",
        b"\xff",
        b"\x1b[31m",
    ] {
        let mut changed = info.clone();
        changed
            .as_dict_mut()
            .unwrap()
            .insert(b"name".to_vec(), Value::Bytes(name.to_vec()));
        seeds.push(bencode::encode(&dictionary(&[(b"info", changed)])));
    }
    let tree = dictionary(&[(
        b"x",
        dictionary(&[(
            b"",
            dictionary(&[
                (b"length", Value::Int(1)),
                (b"pieces root", Value::Bytes(vec![1; 32])),
            ]),
        )]),
    )]);
    for hybrid in [false, true] {
        let mut v2 = info.clone();
        let fields = v2.as_dict_mut().unwrap();
        if !hybrid {
            fields.remove(b"length".as_slice());
            fields.remove(b"pieces".as_slice());
        }
        fields.insert(b"meta version".to_vec(), Value::Int(2));
        fields.insert(b"file tree".to_vec(), tree.clone());
        seeds.push(bencode::encode(&dictionary(&[
            (b"info", v2),
            (b"piece layers", dictionary(&[])),
        ])));
    }
    seeds.extend([
        b"d1:ai1e1:ai2ee".to_vec(),
        b"d1:bi1e1:ai2ee".to_vec(),
        b"999999999999999999999999999999999999:".to_vec(),
        b"i-9223372036854775809e".to_vec(),
        b"i9223372036854775808e".to_vec(),
        b"i-0e".to_vec(),
        b"01:x".to_vec(),
        [vec![b'l'; 140], vec![b'e'; 140]].concat(),
        bencode::encode(&Value::Dict(BTreeMap::new())),
    ]);
    seeds
}

fn next(state: &mut u64) -> usize {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state as usize
}

#[test]
fn seeded_bounded_parser_and_metadata_mutations() {
    const CASES: usize = 50_000;
    let seeds = corpus();
    let mut state = 0x6a09_e667_f3bc_c909_u64;
    let mut parsed = 0;
    let mut documents = 0;
    for case in 0..CASES {
        let mut input = seeds[next(&mut state) % seeds.len()].clone();
        let edits = if case % 16 == 0 {
            0
        } else {
            1 + next(&mut state) % 8
        };
        for _ in 0..edits {
            let position = next(&mut state) % (input.len() + 1);
            let byte = next(&mut state) as u8;
            match next(&mut state) % 5 {
                0 if position < input.len() => input[position] ^= byte,
                1 if input.len() < 4096 => input.insert(position, byte),
                2 if position < input.len() => {
                    input.remove(position);
                }
                3 => input.truncate(position),
                _ if input.len() < 4096 => input.push(byte),
                _ => {}
            }
        }
        let limits = Limits {
            max_bytes: 4096,
            max_depth: 32,
            max_values: 512,
        };
        if let Ok((value, range)) = bencode::parse_with_limits(&input, limits) {
            parsed += 1;
            assert_eq!(bencode::encode(&value), input, "case {case}");
            let raw_info = range.map(|r| input[r].to_vec()).unwrap_or_default();
            let document = metadata::Document { value, raw_info };
            if metadata::validate(&document).is_ok() {
                documents += 1;
                metadata::info_hashes(&document).unwrap();
                metadata::files(&document).unwrap();
                metadata::inspect(&document).unwrap();
            }
        }
    }
    assert!(parsed > 100 && documents > 100);
    eprintln!(
        "mutation cases={CASES}, parsed={parsed}, valid_metadata={documents}, max_input_bytes=4096, max_values=512, max_depth=32"
    );
}

#[test]
fn parser_resource_budgets_reject_before_large_allocations() {
    let limits = Limits {
        max_bytes: 4096,
        max_depth: 16,
        max_values: 64,
    };
    for count in [64, 128, 1024] {
        let input = [b"l".as_slice(), b"0:".repeat(count).as_slice(), b"e"].concat();
        let error = bencode::parse_with_limits(&input, limits).unwrap_err();
        assert!(error.to_string().contains("value count"));
    }
    let deep = [vec![b'l'; 18], vec![b'e'; 18]].concat();
    assert!(
        bencode::parse_with_limits(&deep, limits)
            .unwrap_err()
            .to_string()
            .contains("depth")
    );
    let oversized = vec![b'0'; 4097];
    assert!(
        bencode::parse_with_limits(&oversized, limits)
            .unwrap_err()
            .to_string()
            .contains("byte limit")
    );
    for input in [
        b"18446744073709551615:".as_slice(),
        b"999999999999999999999999999999999999:",
    ] {
        assert!(bencode::parse_with_limits(input, limits).is_err());
    }
}
