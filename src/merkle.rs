use rayon::prelude::*;
use sha2::{Digest, Sha256};

pub const BLOCK_LENGTH: usize = 16 * 1024;
pub type Hash = [u8; 32];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileHashes {
    pub root: Option<Hash>,
    pub piece_layer: Vec<Hash>,
}

/// Hash a file using BEP 52's 16 KiB leaves and zero-hash padding.
/// The piece length must be a power of two of at least 16 KiB.
pub fn hash_file(data: &[u8], piece_length: usize) -> FileHashes {
    hash_file_with_progress(data, piece_length, None)
}

pub(crate) fn hash_file_with_progress(
    data: &[u8],
    piece_length: usize,
    progress: Option<&(dyn Fn(u64) + Sync)>,
) -> FileHashes {
    validate_piece_length(piece_length);
    if data.is_empty() {
        return FileHashes {
            root: None,
            piece_layer: Vec::new(),
        };
    }
    if data.len() <= piece_length {
        let blocks = data.len().div_ceil(BLOCK_LENGTH).next_power_of_two();
        let root = piece_root(data, blocks * BLOCK_LENGTH);
        if let Some(progress) = progress {
            progress(data.len() as u64);
        }
        return FileHashes {
            root: Some(root),
            piece_layer: Vec::new(),
        };
    }
    let piece_layer: Vec<Hash> = match progress {
        Some(progress) => data
            .par_chunks(piece_length)
            .map(|piece| {
                let root = piece_root(piece, piece_length);
                progress(piece.len() as u64);
                root
            })
            .collect(),
        None => data
            .par_chunks(piece_length)
            .map(|piece| piece_root(piece, piece_length))
            .collect(),
    };
    let base_level = (piece_length / BLOCK_LENGTH).trailing_zeros() as usize;
    FileHashes {
        root: Some(combine_root(&piece_layer, base_level)),
        piece_layer,
    }
}

fn validate_piece_length(piece_length: usize) {
    assert!(
        piece_length >= BLOCK_LENGTH && piece_length.is_power_of_two(),
        "piece length must be a power of two of at least 16 KiB"
    );
}

/// Hash one piece, padding missing leaves to the requested piece length.
pub fn piece_root(data: &[u8], piece_length: usize) -> Hash {
    validate_piece_length(piece_length);
    assert!(
        data.len() <= piece_length,
        "piece data exceeds piece length"
    );
    let mut tree = Tree::new();
    for block in data.chunks(BLOCK_LENGTH) {
        tree.push(Sha256::digest(block).into());
    }
    tree.finish((piece_length / BLOCK_LENGTH).trailing_zeros() as usize, 0)
}

/// Combine hashes from a tree layer, padding to the next power of two.
/// `base_level` is that layer's distance above the 16 KiB leaf layer.
pub fn combine_root(hashes: &[Hash], base_level: usize) -> Hash {
    assert!(base_level < usize::BITS as usize, "base layer is too deep");
    let mut tree = Tree::new();
    for hash in hashes {
        tree.push(*hash);
    }
    let levels = hashes.len().max(1).next_power_of_two().trailing_zeros() as usize;
    tree.finish(levels, base_level)
}

fn parent(left: &Hash, right: &Hash) -> Hash {
    let mut hash = Sha256::new();
    hash.update(left);
    hash.update(right);
    hash.finalize().into()
}

// Each occupied slot holds one complete subtree at that height.
struct Tree {
    levels: [Option<Hash>; usize::BITS as usize],
}

impl Tree {
    fn new() -> Self {
        Self {
            levels: [None; usize::BITS as usize],
        }
    }

    fn push(&mut self, mut hash: Hash) {
        for slot in &mut self.levels {
            match slot.take() {
                Some(left) => hash = parent(&left, &hash),
                None => {
                    *slot = Some(hash);
                    return;
                }
            }
        }
        unreachable!("a hash tree cannot exceed the address space");
    }

    fn finish(self, height: usize, base_level: usize) -> Hash {
        let mut zero = [0u8; 32];
        for _ in 0..base_level {
            zero = parent(&zero, &zero);
        }
        let mut carry = None;
        for level in 0..=height {
            let (node, paired) = match (self.levels[level], carry) {
                (Some(left), Some(right)) => (Some(parent(&left, &right)), true),
                (Some(hash), None) | (None, Some(hash)) => (Some(hash), false),
                (None, None) => (None, false),
            };
            if level == height {
                return node.unwrap_or(zero);
            }
            carry = node.map(|hash| if paired { hash } else { parent(&hash, &zero) });
            zero = parent(&zero, &zero);
        }
        unreachable!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(data: &[u8]) -> Option<Hash> {
        if data.is_empty() {
            return None;
        }
        let mut leaves: Vec<Hash> = data
            .chunks(BLOCK_LENGTH)
            .map(|block| Sha256::digest(block).into())
            .collect();
        leaves.resize(leaves.len().next_power_of_two(), [0; 32]);
        while leaves.len() > 1 {
            leaves = leaves
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| {
                    let bytes: Vec<u8> = pair.iter().flatten().copied().collect();
                    Sha256::digest(bytes).into()
                })
                .collect();
        }
        Some(leaves[0])
    }

    #[test]
    fn progress_reports_completed_pieces_including_short_final_data() {
        use std::sync::Mutex;
        let piece_length = BLOCK_LENGTH * 2;
        for length in [0, 1, BLOCK_LENGTH - 1, piece_length, piece_length * 19 + 17] {
            let data: Vec<_> = (0..length).map(|n| (n % 251) as u8).collect();
            let completed = Mutex::new(Vec::new());
            let output = hash_file_with_progress(
                &data,
                piece_length,
                Some(&|bytes| {
                    completed.lock().unwrap().push(bytes);
                }),
            );
            assert_eq!(output.root, reference(&data));
            assert_eq!(output, hash_file(&data, piece_length));
            let mut actual = completed.into_inner().unwrap();
            let mut expected: Vec<_> = data
                .chunks(piece_length)
                .map(|piece| piece.len() as u64)
                .collect();
            actual.sort_unstable();
            expected.sort_unstable();
            assert_eq!(actual, expected);
            assert_eq!(actual.iter().sum::<u64>(), length as u64);
        }
    }

    #[test]
    fn short_block_is_hashed_without_byte_padding() {
        let result = hash_file(b"abc", 64 * 1024);
        assert_eq!(
            hex::encode(result.root.unwrap()),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(result.piece_layer.is_empty());
    }

    #[test]
    fn empty_file_has_no_root_or_layer() {
        assert_eq!(
            hash_file(&[], BLOCK_LENGTH),
            FileHashes {
                root: None,
                piece_layer: vec![]
            }
        );
    }

    #[test]
    fn independent_sha256_vectors() {
        // Generated independently with Python's hashlib and a full padded tree.
        let cases = [
            (
                16385,
                "9d7887c65d577a0237fb3c0998b87b3a62762d03796889a2caea01db914ccbb8",
            ),
            (
                49275,
                "c116a73e0a4cdcc954d1df6d70cbb5cd9b7057c3c770b84e610029c618042dd9",
            ),
            (
                65553,
                "902233581bf92e27521fd7bd02c34ef0c078702c0e7df9f6693c6c4cf83f34a3",
            ),
        ];
        for (length, expected) in cases {
            let data: Vec<u8> = (0..length).map(|i| (i % 251) as u8).collect();
            for piece_length in [BLOCK_LENGTH, 2 * BLOCK_LENGTH, 8 * BLOCK_LENGTH] {
                assert_eq!(
                    hex::encode(hash_file(&data, piece_length).root.unwrap()),
                    expected
                );
            }
        }
    }

    #[test]
    fn roots_match_full_tree_across_boundaries() {
        for blocks in 0..=19 {
            for tail in [0, 1, 8191, BLOCK_LENGTH - 1] {
                let data: Vec<u8> = (0..blocks * BLOCK_LENGTH + tail)
                    .map(|i| (i % 251) as u8)
                    .collect();
                let expected = reference(&data);
                for piece_length in [
                    BLOCK_LENGTH,
                    2 * BLOCK_LENGTH,
                    4 * BLOCK_LENGTH,
                    16 * BLOCK_LENGTH,
                ] {
                    let result = hash_file(&data, piece_length);
                    assert_eq!(
                        result.root,
                        expected,
                        "length={}, piece={piece_length}",
                        data.len()
                    );
                    assert_eq!(
                        result.piece_layer.len(),
                        if data.len() > piece_length {
                            data.len().div_ceil(piece_length)
                        } else {
                            0
                        }
                    );
                }
            }
        }
    }

    #[test]
    fn missing_piece_uses_zero_subtree_at_piece_height() {
        let data = vec![7; BLOCK_LENGTH * 4 + 17];
        let result = hash_file(&data, BLOCK_LENGTH * 2);
        let zero_piece = parent(&[0; 32], &[0; 32]);
        let layer = &result.piece_layer;
        assert_eq!(
            result.root,
            Some(parent(
                &parent(&layer[0], &layer[1]),
                &parent(&layer[2], &zero_piece)
            ))
        );
        assert_eq!(layer[2], parent(&Sha256::digest([7; 17]).into(), &[0; 32]));
    }

    #[test]
    #[should_panic(expected = "piece length must be")]
    fn rejects_invalid_piece_length() {
        hash_file(b"x", BLOCK_LENGTH * 3);
    }
}
