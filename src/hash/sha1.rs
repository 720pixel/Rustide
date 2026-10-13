//! SHA-1 hashing with independent-message SIMD kernels.
//!
//! SHA-1 is used for compatibility with the piece digest format. Equal-length
//! messages are processed together; short or irregular batches use the portable
//! implementation and its available hardware acceleration.

use ::sha1::{Digest, Sha1};

/// Compute the SHA-1 digest of one byte string.
#[inline]
pub fn hash(bytes: &[u8]) -> [u8; 20] {
    Sha1::digest(bytes).into()
}

/// Compute independent SHA-1 digests in input order.
///
/// Runtime feature detection keeps the same binary portable across CPUs. Long,
/// equal-length runs can use eight or sixteen SIMD lanes on x86-64 processors.
pub fn hash_many(pieces: &[&[u8]]) -> Vec<[u8; 20]> {
    let mut output = vec![[0; 20]; pieces.len()];
    hash_many_into(pieces, &mut output);
    output
}

/// Compute independent digests into an existing output buffer.
///
/// # Panics
/// Panics when input and output lengths differ.
pub fn hash_many_into(pieces: &[&[u8]], output: &mut [[u8; 20]]) {
    assert_eq!(pieces.len(), output.len());
    let mut index = 0;
    while index < pieces.len() {
        let length = pieces[index].len();
        let count = pieces[index..]
            .iter()
            .take_while(|p| p.len() == length)
            .count();
        let end = index + count;
        #[cfg(target_arch = "x86_64")]
        if length >= 1024 {
            if std::is_x86_feature_detected!("avx512f") && std::is_x86_feature_detected!("avx512bw")
            {
                while end - index >= 16 {
                    // Detection above establishes every feature used by this kernel.
                    unsafe {
                        x86::wide::hash_batch(
                            pieces[index..index + 16].try_into().unwrap(),
                            &mut output[index..index + 16],
                        );
                    }
                    index += 16;
                }
            }
            if std::is_x86_feature_detected!("avx2") {
                while end - index >= 8 {
                    // Detection above establishes every feature used by this kernel.
                    unsafe {
                        x86::narrow::hash_batch(
                            pieces[index..index + 8].try_into().unwrap(),
                            &mut output[index..index + 8],
                        );
                    }
                    index += 8;
                }
            }
        }
        while index < end {
            output[index] = hash(pieces[index]);
            index += 1;
        }
    }
}

#[cfg(target_arch = "x86_64")]
mod x86 {
    use std::arch::x86_64::*;

    // Expand the rounds inside the streaming loop so the state stays in registers
    // between blocks. Constant schedule indices avoid variable-index vector spills.
    macro_rules! kernel {
        ($lanes:expr, $features:literal, $vector:ty, $splat:ident, $store:ident) => {
            #[target_feature(enable = $features)]
            pub(crate) unsafe fn hash_batch(pieces: &[&[u8]; $lanes], output: &mut [[u8; 20]]) {
                // Callers establish CPU features, equal input lengths, and output count.
                // Each block load stays within an input slice or initialized padding.
                unsafe {
                    let length = pieces[0].len();
                    debug_assert!(pieces.iter().all(|piece| piece.len() == length));
                    debug_assert_eq!(output.len(), $lanes);
                    let mut state = [
                        $splat(0x67452301),
                        $splat(0xefcdab89_u32 as i32),
                        $splat(0x98badcfe_u32 as i32),
                        $splat(0x10325476),
                        $splat(0xc3d2e1f0_u32 as i32),
                    ];
                    let pointers = pieces.map(|piece| piece.as_ptr());
                    let full_length = length / 64 * 64;
                    let remainder = length - full_length;
                    let padded_length = if remainder < 56 { 64 } else { 128 };
                    let bit_length = (length as u64).wrapping_mul(8).to_be_bytes();
                    let mut padding = [[0_u8; 128]; $lanes];
                    for (lane, piece) in pieces.iter().enumerate() {
                        padding[lane][..remainder].copy_from_slice(&piece[full_length..]);
                        padding[lane][remainder] = 0x80;
                        padding[lane][padded_length - 8..padded_length].copy_from_slice(&bit_length);
                    }
                    let tail_pointers = std::array::from_fn(|lane| padding[lane].as_ptr());
                    for offset in (0..full_length + padded_length).step_by(64) {
                        let words = if offset < full_length {
                            load_words(&pointers, offset)
                        } else {
                            load_words(&tail_pointers, offset - full_length)
                        };
                        {
                            let state = &mut state;
                            let mut words = words;
                            let [mut a, mut b, mut c, mut d, mut e] = *state;
                            macro_rules! round {
                                ($a:ident, $b:ident, $c:ident, $d:ident, $e:ident, $t:expr, $f:ident, $k:expr) => {{
                                    if $t >= 16 {
                                        words[$t % 16] = rotate::<1>(xor(
                                            parity(
                                                words[($t + 13) % 16],
                                                words[($t + 8) % 16],
                                                words[($t + 2) % 16],
                                            ),
                                            words[$t % 16],
                                        ));
                                    }
                                    $e = add(
                                        add(
                                            add(rotate::<5>($a), $f($b, $c, $d)),
                                            add($e, words[$t % 16]),
                                        ),
                                        $splat($k),
                                    );
                                    $b = rotate::<30>($b);
                                }};
                            }
                            round!(a, b, c, d, e, 0, choose, 0x5a827999);
                            round!(e, a, b, c, d, 1, choose, 0x5a827999);
                            round!(d, e, a, b, c, 2, choose, 0x5a827999);
                            round!(c, d, e, a, b, 3, choose, 0x5a827999);
                            round!(b, c, d, e, a, 4, choose, 0x5a827999);
                            round!(a, b, c, d, e, 5, choose, 0x5a827999);
                            round!(e, a, b, c, d, 6, choose, 0x5a827999);
                            round!(d, e, a, b, c, 7, choose, 0x5a827999);
                            round!(c, d, e, a, b, 8, choose, 0x5a827999);
                            round!(b, c, d, e, a, 9, choose, 0x5a827999);
                            round!(a, b, c, d, e, 10, choose, 0x5a827999);
                            round!(e, a, b, c, d, 11, choose, 0x5a827999);
                            round!(d, e, a, b, c, 12, choose, 0x5a827999);
                            round!(c, d, e, a, b, 13, choose, 0x5a827999);
                            round!(b, c, d, e, a, 14, choose, 0x5a827999);
                            round!(a, b, c, d, e, 15, choose, 0x5a827999);
                            round!(e, a, b, c, d, 16, choose, 0x5a827999);
                            round!(d, e, a, b, c, 17, choose, 0x5a827999);
                            round!(c, d, e, a, b, 18, choose, 0x5a827999);
                            round!(b, c, d, e, a, 19, choose, 0x5a827999);
                            round!(a, b, c, d, e, 20, parity, 0x6ed9eba1);
                            round!(e, a, b, c, d, 21, parity, 0x6ed9eba1);
                            round!(d, e, a, b, c, 22, parity, 0x6ed9eba1);
                            round!(c, d, e, a, b, 23, parity, 0x6ed9eba1);
                            round!(b, c, d, e, a, 24, parity, 0x6ed9eba1);
                            round!(a, b, c, d, e, 25, parity, 0x6ed9eba1);
                            round!(e, a, b, c, d, 26, parity, 0x6ed9eba1);
                            round!(d, e, a, b, c, 27, parity, 0x6ed9eba1);
                            round!(c, d, e, a, b, 28, parity, 0x6ed9eba1);
                            round!(b, c, d, e, a, 29, parity, 0x6ed9eba1);
                            round!(a, b, c, d, e, 30, parity, 0x6ed9eba1);
                            round!(e, a, b, c, d, 31, parity, 0x6ed9eba1);
                            round!(d, e, a, b, c, 32, parity, 0x6ed9eba1);
                            round!(c, d, e, a, b, 33, parity, 0x6ed9eba1);
                            round!(b, c, d, e, a, 34, parity, 0x6ed9eba1);
                            round!(a, b, c, d, e, 35, parity, 0x6ed9eba1);
                            round!(e, a, b, c, d, 36, parity, 0x6ed9eba1);
                            round!(d, e, a, b, c, 37, parity, 0x6ed9eba1);
                            round!(c, d, e, a, b, 38, parity, 0x6ed9eba1);
                            round!(b, c, d, e, a, 39, parity, 0x6ed9eba1);
                            round!(a, b, c, d, e, 40, majority, 0x8f1bbcdc_u32 as i32);
                            round!(e, a, b, c, d, 41, majority, 0x8f1bbcdc_u32 as i32);
                            round!(d, e, a, b, c, 42, majority, 0x8f1bbcdc_u32 as i32);
                            round!(c, d, e, a, b, 43, majority, 0x8f1bbcdc_u32 as i32);
                            round!(b, c, d, e, a, 44, majority, 0x8f1bbcdc_u32 as i32);
                            round!(a, b, c, d, e, 45, majority, 0x8f1bbcdc_u32 as i32);
                            round!(e, a, b, c, d, 46, majority, 0x8f1bbcdc_u32 as i32);
                            round!(d, e, a, b, c, 47, majority, 0x8f1bbcdc_u32 as i32);
                            round!(c, d, e, a, b, 48, majority, 0x8f1bbcdc_u32 as i32);
                            round!(b, c, d, e, a, 49, majority, 0x8f1bbcdc_u32 as i32);
                            round!(a, b, c, d, e, 50, majority, 0x8f1bbcdc_u32 as i32);
                            round!(e, a, b, c, d, 51, majority, 0x8f1bbcdc_u32 as i32);
                            round!(d, e, a, b, c, 52, majority, 0x8f1bbcdc_u32 as i32);
                            round!(c, d, e, a, b, 53, majority, 0x8f1bbcdc_u32 as i32);
                            round!(b, c, d, e, a, 54, majority, 0x8f1bbcdc_u32 as i32);
                            round!(a, b, c, d, e, 55, majority, 0x8f1bbcdc_u32 as i32);
                            round!(e, a, b, c, d, 56, majority, 0x8f1bbcdc_u32 as i32);
                            round!(d, e, a, b, c, 57, majority, 0x8f1bbcdc_u32 as i32);
                            round!(c, d, e, a, b, 58, majority, 0x8f1bbcdc_u32 as i32);
                            round!(b, c, d, e, a, 59, majority, 0x8f1bbcdc_u32 as i32);
                            round!(a, b, c, d, e, 60, parity, 0xca62c1d6_u32 as i32);
                            round!(e, a, b, c, d, 61, parity, 0xca62c1d6_u32 as i32);
                            round!(d, e, a, b, c, 62, parity, 0xca62c1d6_u32 as i32);
                            round!(c, d, e, a, b, 63, parity, 0xca62c1d6_u32 as i32);
                            round!(b, c, d, e, a, 64, parity, 0xca62c1d6_u32 as i32);
                            round!(a, b, c, d, e, 65, parity, 0xca62c1d6_u32 as i32);
                            round!(e, a, b, c, d, 66, parity, 0xca62c1d6_u32 as i32);
                            round!(d, e, a, b, c, 67, parity, 0xca62c1d6_u32 as i32);
                            round!(c, d, e, a, b, 68, parity, 0xca62c1d6_u32 as i32);
                            round!(b, c, d, e, a, 69, parity, 0xca62c1d6_u32 as i32);
                            round!(a, b, c, d, e, 70, parity, 0xca62c1d6_u32 as i32);
                            round!(e, a, b, c, d, 71, parity, 0xca62c1d6_u32 as i32);
                            round!(d, e, a, b, c, 72, parity, 0xca62c1d6_u32 as i32);
                            round!(c, d, e, a, b, 73, parity, 0xca62c1d6_u32 as i32);
                            round!(b, c, d, e, a, 74, parity, 0xca62c1d6_u32 as i32);
                            round!(a, b, c, d, e, 75, parity, 0xca62c1d6_u32 as i32);
                            round!(e, a, b, c, d, 76, parity, 0xca62c1d6_u32 as i32);
                            round!(d, e, a, b, c, 77, parity, 0xca62c1d6_u32 as i32);
                            round!(c, d, e, a, b, 78, parity, 0xca62c1d6_u32 as i32);
                            round!(b, c, d, e, a, 79, parity, 0xca62c1d6_u32 as i32);
                            state[0] = add(state[0], a);
                            state[1] = add(state[1], b);
                            state[2] = add(state[2], c);
                            state[3] = add(state[3], d);
                            state[4] = add(state[4], e);
                        }
                    }
                    let mut values = [[0_u32; $lanes]; 5];
                    for word in 0..5 {
                        $store(values[word].as_mut_ptr().cast(), state[word]);
                    }
                    for lane in 0..$lanes {
                        for word in 0..5 {
                            output[lane][word * 4..word * 4 + 4].copy_from_slice(&values[word][lane].to_be_bytes());
                        }
                    }
                }
            }
        };
    }

    pub(super) mod narrow {
        use super::*;

        #[inline]
        #[target_feature(enable = "avx2")]
        fn add(a: __m256i, b: __m256i) -> __m256i {
            _mm256_add_epi32(a, b)
        }
        #[inline]
        #[target_feature(enable = "avx2")]
        fn xor(a: __m256i, b: __m256i) -> __m256i {
            _mm256_xor_si256(a, b)
        }
        #[inline]
        #[target_feature(enable = "avx2")]
        fn rotate<const N: i32>(a: __m256i) -> __m256i {
            // Variable-count intrinsics allow stable const generics without const arithmetic.
            _mm256_or_si256(
                _mm256_sll_epi32(a, _mm_cvtsi32_si128(N)),
                _mm256_srl_epi32(a, _mm_cvtsi32_si128(32 - N)),
            )
        }
        #[inline]
        #[target_feature(enable = "avx2")]
        fn choose(b: __m256i, c: __m256i, d: __m256i) -> __m256i {
            xor(d, _mm256_and_si256(b, xor(c, d)))
        }
        #[inline]
        #[target_feature(enable = "avx2")]
        fn parity(b: __m256i, c: __m256i, d: __m256i) -> __m256i {
            xor(xor(b, c), d)
        }
        #[inline]
        #[target_feature(enable = "avx2")]
        fn majority(b: __m256i, c: __m256i, d: __m256i) -> __m256i {
            _mm256_or_si256(
                _mm256_and_si256(b, c),
                _mm256_and_si256(_mm256_or_si256(b, c), d),
            )
        }

        #[inline]
        #[target_feature(enable = "avx2")]
        unsafe fn load_words(pointers: &[*const u8; 8], offset: usize) -> [__m256i; 16] {
            unsafe {
                let mut words = [_mm256_setzero_si256(); 16];
                let byte_swap = _mm256_broadcastsi128_si256(_mm_setr_epi32(
                    0x00010203, 0x04050607, 0x08090a0b, 0x0c0d0e0f,
                ));
                for half in 0..2 {
                    let rows: [__m256i; 8] = std::array::from_fn(|lane| {
                        _mm256_shuffle_epi8(
                            _mm256_loadu_si256(pointers[lane].add(offset + half * 32).cast()),
                            byte_swap,
                        )
                    });
                    let mut pairs = [_mm256_setzero_si256(); 8];
                    for group in 0..4 {
                        pairs[group * 2] =
                            _mm256_unpacklo_epi32(rows[group * 2], rows[group * 2 + 1]);
                        pairs[group * 2 + 1] =
                            _mm256_unpackhi_epi32(rows[group * 2], rows[group * 2 + 1]);
                    }
                    let mut quads = [_mm256_setzero_si256(); 8];
                    for group in 0..2 {
                        let i = group * 4;
                        quads[i] = _mm256_unpacklo_epi64(pairs[i], pairs[i + 2]);
                        quads[i + 1] = _mm256_unpackhi_epi64(pairs[i], pairs[i + 2]);
                        quads[i + 2] = _mm256_unpacklo_epi64(pairs[i + 1], pairs[i + 3]);
                        quads[i + 3] = _mm256_unpackhi_epi64(pairs[i + 1], pairs[i + 3]);
                    }
                    for word in 0..4 {
                        words[half * 8 + word] =
                            _mm256_permute2x128_si256::<0x20>(quads[word], quads[word + 4]);
                        words[half * 8 + word + 4] =
                            _mm256_permute2x128_si256::<0x31>(quads[word], quads[word + 4]);
                    }
                }
                words
            }
        }
        kernel!(8, "avx2", __m256i, _mm256_set1_epi32, _mm256_storeu_si256);
    }

    pub(super) mod wide {
        use super::*;

        #[inline]
        #[target_feature(enable = "avx512f,avx512bw")]
        fn add(a: __m512i, b: __m512i) -> __m512i {
            _mm512_add_epi32(a, b)
        }
        #[inline]
        #[target_feature(enable = "avx512f,avx512bw")]
        fn xor(a: __m512i, b: __m512i) -> __m512i {
            _mm512_xor_si512(a, b)
        }
        #[inline]
        #[target_feature(enable = "avx512f,avx512bw")]
        fn rotate<const N: i32>(a: __m512i) -> __m512i {
            _mm512_rol_epi32::<N>(a)
        }
        #[inline]
        #[target_feature(enable = "avx512f,avx512bw")]
        fn choose(b: __m512i, c: __m512i, d: __m512i) -> __m512i {
            _mm512_ternarylogic_epi32::<0xca>(b, c, d)
        }
        #[inline]
        #[target_feature(enable = "avx512f,avx512bw")]
        fn parity(b: __m512i, c: __m512i, d: __m512i) -> __m512i {
            _mm512_ternarylogic_epi32::<0x96>(b, c, d)
        }
        #[inline]
        #[target_feature(enable = "avx512f,avx512bw")]
        fn majority(b: __m512i, c: __m512i, d: __m512i) -> __m512i {
            _mm512_ternarylogic_epi32::<0xe8>(b, c, d)
        }

        #[inline]
        #[target_feature(enable = "avx512f,avx512bw")]
        unsafe fn load_words(pointers: &[*const u8; 16], offset: usize) -> [__m512i; 16] {
            unsafe {
                let byte_swap = _mm512_broadcast_i32x4(_mm_setr_epi32(
                    0x00010203, 0x04050607, 0x08090a0b, 0x0c0d0e0f,
                ));
                let rows: [__m512i; 16] = std::array::from_fn(|lane| {
                    _mm512_shuffle_epi8(
                        _mm512_loadu_si512(pointers[lane].add(offset).cast()),
                        byte_swap,
                    )
                });
                let mut pairs = [_mm512_setzero_si512(); 16];
                for group in 0..8 {
                    pairs[group * 2] = _mm512_unpacklo_epi32(rows[group * 2], rows[group * 2 + 1]);
                    pairs[group * 2 + 1] =
                        _mm512_unpackhi_epi32(rows[group * 2], rows[group * 2 + 1]);
                }
                let mut quads = [_mm512_setzero_si512(); 16];
                for group in 0..4 {
                    let i = group * 4;
                    quads[i] = _mm512_unpacklo_epi64(pairs[i], pairs[i + 2]);
                    quads[i + 1] = _mm512_unpackhi_epi64(pairs[i], pairs[i + 2]);
                    quads[i + 2] = _mm512_unpacklo_epi64(pairs[i + 1], pairs[i + 3]);
                    quads[i + 3] = _mm512_unpackhi_epi64(pairs[i + 1], pairs[i + 3]);
                }
                let mut words = [_mm512_setzero_si512(); 16];
                for word in 0..4 {
                    let lo_a = _mm512_shuffle_i32x4::<0x44>(quads[word], quads[word + 4]);
                    let lo_b = _mm512_shuffle_i32x4::<0x44>(quads[word + 8], quads[word + 12]);
                    let hi_a = _mm512_shuffle_i32x4::<0xee>(quads[word], quads[word + 4]);
                    let hi_b = _mm512_shuffle_i32x4::<0xee>(quads[word + 8], quads[word + 12]);
                    words[word] = _mm512_shuffle_i32x4::<0x88>(lo_a, lo_b);
                    words[word + 4] = _mm512_shuffle_i32x4::<0xdd>(lo_a, lo_b);
                    words[word + 8] = _mm512_shuffle_i32x4::<0x88>(hi_a, hi_b);
                    words[word + 12] = _mm512_shuffle_i32x4::<0xdd>(hi_a, hi_b);
                }
                words
            }
        }
        kernel!(
            16,
            "avx512f,avx512bw",
            __m512i,
            _mm512_set1_epi32,
            _mm512_storeu_si512
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(digest: [u8; 20]) -> String {
        digest.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn known_vectors() {
        for (message, expected) in [
            ("", "da39a3ee5e6b4b0d3255bfef95601890afd80709"),
            ("abc", "a9993e364706816aba3e25717850c26c9cd0d89d"),
            (
                "abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq",
                "84983e441c3bd26ebaae4aa1f95129e5e54670f1",
            ),
        ] {
            assert_eq!(hex(hash(message.as_bytes())), expected);
        }
        assert_eq!(
            hex(hash(&vec![b'a'; 1_000_000])),
            "34aa973cd4c4daa4f61eeb2bdbad27316534016f"
        );
    }

    fn messages(count: usize, length: usize) -> Vec<Vec<u8>> {
        (0..count)
            .map(|lane| {
                (0..length)
                    .map(|i| ((i.wrapping_mul(157) ^ (i / 7) ^ lane.wrapping_mul(73)) & 255) as u8)
                    .collect()
            })
            .collect()
    }

    #[test]
    fn dispatch_matches_individual_digests() {
        assert!(hash_many(&[]).is_empty());
        for count in [1, 7, 8, 9, 15, 16, 17, 24, 31, 32, 33] {
            for length in [0, 55, 56, 63, 64, 65, 127, 128, 1023, 1024, 1025, 4096] {
                let owned = messages(count, length);
                let refs: Vec<_> = owned.iter().map(Vec::as_slice).collect();
                assert_eq!(
                    hash_many(&refs),
                    refs.iter().map(|piece| hash(piece)).collect::<Vec<_>>()
                );
            }
        }
        let owned: Vec<_> = (0..41)
            .map(|i| messages(1, i * 53).pop().unwrap())
            .collect();
        let refs: Vec<_> = owned.iter().map(Vec::as_slice).collect();
        assert_eq!(
            hash_many(&refs),
            refs.iter().map(|piece| hash(piece)).collect::<Vec<_>>()
        );
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn hardware_kernels_cover_padding_and_unaligned_inputs() {
        for length in [
            0, 1, 3, 55, 56, 57, 63, 64, 65, 119, 120, 127, 128, 129, 1024, 4097,
        ] {
            let owned = messages(16, length + 1);
            // The one-byte offset exercises unaligned SIMD loads.
            let refs: Vec<_> = owned.iter().map(|piece| &piece[1..]).collect();
            let expected: Vec<_> = refs.iter().map(|piece| hash(piece)).collect();
            if std::is_x86_feature_detected!("avx2") {
                let mut actual = [[0_u8; 20]; 8];
                unsafe {
                    x86::narrow::hash_batch(refs[..8].try_into().unwrap(), &mut actual);
                }
                assert_eq!(actual.as_slice(), &expected[..8], "AVX2 length {length}");
            }
            if std::is_x86_feature_detected!("avx512f") && std::is_x86_feature_detected!("avx512bw")
            {
                let mut actual = [[0_u8; 20]; 16];
                unsafe {
                    x86::wide::hash_batch(refs.as_slice().try_into().unwrap(), &mut actual);
                }
                assert_eq!(actual.as_slice(), expected, "AVX512 length {length}");
            }
        }
    }
}
