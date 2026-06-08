// Big Bounce codec: LZ77 + per-block Huffman with byte-shuffle transforms.
// Self-contained, zero external dependencies. Extracted from the reference
// benchmark implementation and exposed as a reusable library module.
#![allow(dead_code)]
#![allow(unused_assignments)]


// Distance code table (deflate-style, inspired by FCD factorization)
// code → (base_distance, extra_bits_count)
const DIST_CODE_TABLE: [(u16, u8); 32] = [
    (0, 0), (1, 0), (2, 0), (3, 0),        // codes 0-3: dist 1-4 exact
    (4, 1), (6, 1),                          // codes 4-5: 1 extra bit
    (8, 2), (12, 2),                         // codes 6-7: 2 extra bits
    (16, 3), (24, 3),                        // codes 8-9: 3 extra bits
    (32, 4), (48, 4),                        // codes 10-11: 4 extra bits
    (64, 5), (96, 5),                        // codes 12-13: 5 extra bits
    (128, 6), (192, 6),                      // codes 14-15: 6 extra bits
    (256, 7), (384, 7),                      // codes 16-17: 7 extra bits
    (512, 8), (768, 8),                      // codes 18-19: 8 extra bits
    (1024, 9), (1536, 9),                    // codes 20-21: 9 extra bits
    (2048, 10), (3072, 10),                  // codes 22-23: 10 extra bits
    (4096, 11), (6144, 11),                  // codes 24-25: 11 extra bits
    (8192, 12), (12288, 12),                 // codes 26-27: 12 extra bits
    (16384, 13), (24576, 13),                // codes 28-29: 13 extra bits
    (32768, 14), (49152, 14),                // codes 30-31: 14 extra bits (64KB window)
];

const DIST_LOOKUP: [u8; 65536] = {
    let mut lookup = [0u8; 65536];
    let mut j = 0;
    while j < 32 {
        let start = DIST_CODE_TABLE[j].0 as usize;
        let end = if j + 1 < 32 {
            DIST_CODE_TABLE[j + 1].0 as usize
        } else {
            65536
        };
        let mut i = start;
        while i < end {
            lookup[i] = j as u8;
            i += 1;
        }
        j += 1;
    }
    lookup
};

fn dist_to_code(dist: u16) -> (u8, u16, u8) {
    let code = DIST_LOOKUP[dist as usize];
    let (base, extra_bits) = DIST_CODE_TABLE[code as usize];
    (code, dist - base, extra_bits)
}

fn code_to_dist(code: u8, extra: u16) -> u16 {
    DIST_CODE_TABLE[code as usize].0 + extra
}

// Length code table (deflate-style, inspired by FCD factorization)
// code → (base_length, extra_bits_count)
const LEN_CODE_TABLE: [(u16, u8); 29] = [
    (3, 0), (4, 0), (5, 0), (6, 0), (7, 0), (8, 0), (9, 0), (10, 0), // codes 0-7: length 3-10 exact
    (11, 1), (13, 1), (15, 1), (17, 1),                             // codes 8-11: 1 extra bit
    (19, 2), (23, 2), (27, 2), (31, 2),                             // codes 12-15: 2 extra bits
    (35, 3), (43, 3), (51, 3), (59, 3),                             // codes 16-19: 3 extra bits
    (67, 4), (83, 4), (99, 4), (115, 4),                            // codes 20-23: 4 extra bits
    (131, 5), (163, 5), (195, 5), (227, 5),                           // codes 24-27: 5 extra bits
    (258, 0),                                                       // code 28: length 258 exact
];

const LEN_LOOKUP: [u8; 259] = build_len_lookup();

const fn build_len_lookup() -> [u8; 259] {
    let mut lookup = [0u8; 259];
    let mut i = 0;
    while i < 259 {
        let length = i as u16;
        if length >= 258 {
            lookup[i] = 28;
        } else {
            let mut code = 0;
            let mut c = 0;
            while c < 28 {
                if length >= LEN_CODE_TABLE[c].0 {
                    code = c as u8;
                }
                c += 1;
            }
            lookup[i] = code;
        }
        i += 1;
    }
    lookup
}

fn len_to_code(length: u16) -> (u8, u16, u8) {
    if length >= 258 {
        return (28, 0, 0);
    }
    let code = LEN_LOOKUP[length as usize];
    let (base, n_extra) = LEN_CODE_TABLE[code as usize];
    (code, length - base, n_extra)
}

fn code_to_len(code: u8, extra: u16) -> u16 {
    if code > 28 {
        return 0;
    }
    LEN_CODE_TABLE[code as usize].0 + extra
}

fn package_merge_sorted_in_place(max_length: usize, num_codes: usize, a: &mut [usize]) -> usize {
    if num_codes == 0 || max_length == 0 {
        return 0;
    }
    if num_codes <= 2 {
        a[0] = 1;
        if num_codes == 2 {
            a[1] = 1;
        }
        return 1;
    }

    if max_length > 63 {
        return 0;
    }

    let encoding_limit = 1u64 << max_length;
    if encoding_limit < num_codes as u64 {
        return 0;
    }

    let max_buffer = 2 * num_codes;
    let mut current = vec![0usize; max_buffer];
    let mut previous = vec![0usize; max_buffer];
    let mut is_merged = vec![0u64; max_buffer];

    for i in 0..num_codes {
        previous[i] = a[i];
    }
    let mut num_previous = num_codes;
    let num_relevant = 2 * num_codes - 2;

    let mut mask = 1u64;
    for _bits in (1..max_length).rev() {
        num_previous &= !1;

        current[0] = a[0];
        current[1] = a[1];
        let mut sum = current[0] + current[1];

        let mut num_current = 2;
        let mut num_hist = num_current;
        let mut num_merged = 0;

        loop {
            if num_hist < num_codes && a[num_hist] <= sum {
                current[num_current] = a[num_hist];
                num_current += 1;
                num_hist += 1;
                continue;
            }

            is_merged[num_current] |= mask;
            current[num_current] = sum;
            num_current += 1;
            num_merged += 1;

            if num_merged * 2 >= num_previous {
                break;
            }

            sum = previous[num_merged * 2] + previous[num_merged * 2 + 1];
        }

        while num_hist < num_codes {
            current[num_current] = a[num_hist];
            num_current += 1;
            num_hist += 1;
        }

        mask <<= 1;

        if num_previous >= num_relevant {
            let mut keep_going = false;
            for i in (1..num_relevant).rev() {
                if previous[i] != current[i] {
                    keep_going = true;
                    break;
                }
            }
            if !keep_going {
                break;
            }
        }

        std::mem::swap(&mut previous, &mut current);
        num_previous = num_current;
    }

    mask >>= 1;

    for i in 0..num_codes {
        a[i] = 0;
    }

    let mut num_analyze = num_relevant;
    while mask != 0 {
        let mut num_merged = 0;
        a[0] += 1;
        a[1] += 1;
        let mut symbol = 2;

        for i in symbol..num_analyze {
            if (is_merged[i] & mask) == 0 {
                a[symbol] += 1;
                symbol += 1;
            } else {
                num_merged += 1;
            }
        }

        num_analyze = 2 * num_merged;
        mask >>= 1;
    }

    for i in 0..num_analyze {
        a[i] += 1;
    }

    a[0]
}

struct KeyValue {
    key: usize,
    value: usize,
}

fn package_merge(
    max_length: usize,
    num_codes: usize,
    histogram: &[usize],
    code_lengths: &mut [u8],
) -> usize {
    for i in 0..num_codes {
        code_lengths[i] = 0;
    }

    let mut num_nonzero = 0;
    for i in 0..num_codes {
        if histogram[i] != 0 {
            num_nonzero += 1;
        }
    }

    if num_nonzero == 0 {
        return 0;
    }

    let mut mapping = Vec::with_capacity(num_nonzero);
    for i in 0..num_codes {
        if histogram[i] != 0 {
            mapping.push(KeyValue {
                key: histogram[i],
                value: i,
            });
        }
    }

    mapping.sort_by_key(|kv| kv.key);

    let mut sorted = Vec::with_capacity(num_nonzero);
    for i in 0..num_nonzero {
        sorted.push(mapping[i].key);
    }

    let result = package_merge_sorted_in_place(max_length, num_nonzero, &mut sorted);

    for i in 0..num_nonzero {
        code_lengths[mapping[i].value] = sorted[i] as u8;
    }

    result
}

const HUFF_UINT16_ALPHABET_SIZE: usize = 286;

fn huff_encode_uint16(data: &[u16]) -> Option<Vec<u8>> {
    if data.len() < 256 {
        return None;
    }

    let mut freq = [0usize; HUFF_UINT16_ALPHABET_SIZE];
    for &b in data {
        if (b as usize) < HUFF_UINT16_ALPHABET_SIZE {
            freq[b as usize] += 1;
        }
    }

    let mut unique_count = 0;
    for &f in &freq {
        if f > 0 {
            unique_count += 1;
        }
    }
    if unique_count <= 1 {
        return None;
    }

    let mut code_lens = [0u8; HUFF_UINT16_ALPHABET_SIZE];
    package_merge(15, HUFF_UINT16_ALPHABET_SIZE, &freq, &mut code_lens);

    let mut bl_count = [0usize; 16];
    let mut max_bits = 0;
    for &cl in &code_lens {
        if cl > 0 {
            bl_count[cl as usize] += 1;
            if cl as usize > max_bits {
                max_bits = cl as usize;
            }
        }
    }

    let mut next_code = [0u32; 16];
    let mut code = 0u32;
    for bits in 1..=max_bits {
        code = (code + bl_count[bits - 1] as u32) << 1;
        next_code[bits] = code;
    }

    let mut codes = [0u32; HUFF_UINT16_ALPHABET_SIZE];
    let mut code_widths = [0u8; HUFF_UINT16_ALPHABET_SIZE];
    for sym in 0..HUFF_UINT16_ALPHABET_SIZE {
        let cl = code_lens[sym] as usize;
        if cl > 0 {
            codes[sym] = next_code[cl];
            code_widths[sym] = cl as u8;
            next_code[cl] += 1;
        }
    }

    let header_len = 1 + 143;
    let mut out = Vec::with_capacity(data.len() + header_len);
    out.push(max_bits as u8);
    for i in (0..HUFF_UINT16_ALPHABET_SIZE).step_by(2) {
        let hi = code_lens[i] & 0x0F;
        let lo = code_lens[i + 1] & 0x0F;
        out.push((hi << 4) | lo);
    }

    let mut bit_buf = 0u64;
    let mut bit_pos = 0usize;
    for &b in data {
        let cl = code_widths[b as usize] as usize;
        let c = codes[b as usize] as u64;
        bit_buf = (bit_buf << cl) | c;
        bit_pos += cl;
        while bit_pos >= 8 {
            bit_pos -= 8;
            out.push((bit_buf >> bit_pos) as u8);
            bit_buf &= (1u64 << bit_pos) - 1;
        }
    }

    if bit_pos > 0 {
        out.push((bit_buf << (8 - bit_pos)) as u8);
    }

    Some(out)
}

struct BitReader<'a> {
    data: &'a [u8],
    idx: usize,
    bit_buf: u64,
    bits_left: usize,
}

impl<'a> BitReader<'a> {
    #[inline(always)]
    fn new(data: &'a [u8]) -> Self {
        let mut br = BitReader {
            data,
            idx: 0,
            bit_buf: 0,
            bits_left: 0,
        };
        br.fill();
        br
    }

    #[inline(always)]
    fn fill(&mut self) {
        while self.bits_left <= 56 && self.idx < self.data.len() {
            let needed_bytes = (64 - self.bits_left) / 8;
            let available = self.data.len() - self.idx;
            let to_read = std::cmp::min(needed_bytes, available);
            if to_read >= 4 {
                let bytes = [
                    self.data[self.idx],
                    self.data[self.idx + 1],
                    self.data[self.idx + 2],
                    self.data[self.idx + 3],
                ];
                let val = u32::from_be_bytes(bytes);
                self.bit_buf = (self.bit_buf << 32) | (val as u64);
                self.bits_left += 32;
                self.idx += 4;
            } else {
                for _ in 0..to_read {
                    self.bit_buf = (self.bit_buf << 8) | (self.data[self.idx] as u64);
                    self.bits_left += 8;
                    self.idx += 1;
                }
            }
        }
    }

    #[inline(always)]
    fn peek(&mut self, n: usize) -> u32 {
        if self.bits_left < n {
            self.fill();
        }
        if self.bits_left == 0 {
            return 0;
        }
        if self.bits_left >= n {
            ((self.bit_buf >> (self.bits_left - n)) & ((1 << n) - 1)) as u32
        } else {
            ((self.bit_buf & ((1 << self.bits_left) - 1)) << (n - self.bits_left)) as u32
        }
    }

    #[inline(always)]
    fn consume(&mut self, n: usize) {
        if self.bits_left >= n {
            self.bits_left -= n;
            self.bit_buf &= (1 << self.bits_left) - 1;
        } else {
            self.bits_left = 0;
            self.bit_buf = 0;
        }
    }
}

fn huff_decode_uint16(data: &[u8], expected_len: usize) -> Result<Vec<u16>, String> {
    let header_len = 1 + 143;
    if data.len() < header_len {
        return Err("huffUint16: data too short for header".to_string());
    }

    let max_bits = data[0] as usize;
    if max_bits > 15 || max_bits == 0 {
        return Err(format!("huffUint16: invalid maxBits {}", max_bits));
    }

    let mut code_lens = [0u8; HUFF_UINT16_ALPHABET_SIZE];
    for i in 0..143 {
        code_lens[i * 2] = (data[1 + i] >> 4) & 0x0F;
        code_lens[i * 2 + 1] = data[1 + i] & 0x0F;
    }
    let bitstream = &data[header_len..];

    let mut bl_count = [0usize; 16];
    for &cl in &code_lens {
        if cl > 0 {
            bl_count[cl as usize] += 1;
        }
    }

    let mut next_code = [0u32; 16];
    let mut code = 0u32;
    for bits in 1..=max_bits {
        code = (code + bl_count[bits - 1] as u32) << 1;
        next_code[bits] = code;
    }

    #[derive(Clone, Copy, Default)]
    struct HuffEntry {
        sym: u16,
        bits: u8,
    }

    let table_size = 1 << max_bits;
    let mut table = vec![HuffEntry::default(); table_size];
    for sym in 0..HUFF_UINT16_ALPHABET_SIZE {
        let cl = code_lens[sym] as usize;
        if cl == 0 {
            continue;
        }
        let c = next_code[cl];
        next_code[cl] += 1;
        let pad = max_bits - cl;
        for p in 0..(1 << pad) {
            let idx = ((c << pad) | p as u32) as usize;
            if idx < table_size {
                table[idx] = HuffEntry {
                    sym: sym as u16,
                    bits: cl as u8,
                };
            }
        }
    }

    let mut out = Vec::with_capacity(expected_len);
    let mut reader = BitReader::new(bitstream);

    while out.len() < expected_len {
        let peek = reader.peek(max_bits) as usize;
        let entry = table[peek];
        if entry.bits == 0 {
            return Err("huffUint16: invalid code".to_string());
        }
        out.push(entry.sym);
        reader.consume(entry.bits as usize);
    }

    if out.len() != expected_len {
        return Err(format!("huffUint16: got {} want {}", out.len(), expected_len));
    }
    Ok(out)
}

const HUFF_ALPHABET_SIZE: usize = 256;

fn huff_encode(data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < 256 {
        return None;
    }

    let mut freq = [0usize; HUFF_ALPHABET_SIZE];
    for &b in data {
        freq[b as usize] += 1;
    }

    let mut unique_count = 0;
    for &f in &freq {
        if f > 0 {
            unique_count += 1;
        }
    }
    if unique_count <= 1 {
        return None;
    }

    let mut code_lens = [0u8; HUFF_ALPHABET_SIZE];
    package_merge(15, HUFF_ALPHABET_SIZE, &freq, &mut code_lens);

    let mut bl_count = [0usize; 16];
    let mut max_bits = 0;
    for &cl in &code_lens {
        if cl > 0 {
            bl_count[cl as usize] += 1;
            if cl as usize > max_bits {
                max_bits = cl as usize;
            }
        }
    }

    let mut next_code = [0u32; 16];
    let mut code = 0u32;
    for bits in 1..=max_bits {
        code = (code + bl_count[bits - 1] as u32) << 1;
        next_code[bits] = code;
    }

    let mut codes = [0u32; HUFF_ALPHABET_SIZE];
    let mut code_widths = [0u8; HUFF_ALPHABET_SIZE];
    for sym in 0..HUFF_ALPHABET_SIZE {
        let cl = code_lens[sym] as usize;
        if cl > 0 {
            codes[sym] = next_code[cl];
            code_widths[sym] = cl as u8;
            next_code[cl] += 1;
        }
    }

    let header_len = 1 + 128;
    let mut out = Vec::with_capacity(data.len() + header_len);
    out.push(max_bits as u8);
    for i in (0..HUFF_ALPHABET_SIZE).step_by(2) {
        let hi = code_lens[i] & 0x0F;
        let lo = code_lens[i + 1] & 0x0F;
        out.push((hi << 4) | lo);
    }

    let mut bit_buf = 0u64;
    let mut bit_pos = 0usize;
    for &b in data {
        let cl = code_widths[b as usize] as usize;
        let c = codes[b as usize] as u64;
        bit_buf = (bit_buf << cl) | c;
        bit_pos += cl;
        while bit_pos >= 8 {
            bit_pos -= 8;
            out.push((bit_buf >> bit_pos) as u8);
            bit_buf &= (1u64 << bit_pos) - 1;
        }
    }

    if bit_pos > 0 {
        out.push((bit_buf << (8 - bit_pos)) as u8);
    }

    Some(out)
}

fn huff_decode(data: &[u8], expected_len: usize) -> Result<Vec<u8>, String> {
    let header_len = 1 + 128;
    if data.len() < header_len {
        return Err("huff: data too short for header".to_string());
    }

    let max_bits = data[0] as usize;
    if max_bits > 15 || max_bits == 0 {
        return Err(format!("huff: invalid maxBits {}", max_bits));
    }

    let mut code_lens = [0u8; HUFF_ALPHABET_SIZE];
    for i in 0..128 {
        code_lens[i * 2] = (data[1 + i] >> 4) & 0x0F;
        code_lens[i * 2 + 1] = data[1 + i] & 0x0F;
    }
    let bitstream = &data[header_len..];

    let mut bl_count = [0usize; 16];
    for &cl in &code_lens {
        if cl > 0 {
            bl_count[cl as usize] += 1;
        }
    }

    let mut next_code = [0u32; 16];
    let mut code = 0u32;
    for bits in 1..=max_bits {
        code = (code + bl_count[bits - 1] as u32) << 1;
        next_code[bits] = code;
    }

    #[derive(Clone, Copy, Default)]
    struct HuffEntry {
        sym: u8,
        bits: u8,
    }

    let table_size = 1 << max_bits;
    let mut table = vec![HuffEntry::default(); table_size];
    for sym in 0..HUFF_ALPHABET_SIZE {
        let cl = code_lens[sym] as usize;
        if cl == 0 {
            continue;
        }
        let c = next_code[cl];
        next_code[cl] += 1;
        let pad = max_bits - cl;
        for p in 0..(1 << pad) {
            let idx = ((c << pad) | p as u32) as usize;
            if idx < table_size {
                table[idx] = HuffEntry {
                    sym: sym as u8,
                    bits: cl as u8,
                };
            }
        }
    }

    let mut out = Vec::with_capacity(expected_len);
    let mut reader = BitReader::new(bitstream);

    while out.len() < expected_len {
        let peek = reader.peek(max_bits) as usize;
        let entry = table[peek];
        if entry.bits == 0 {
            return Err("huff: invalid code".to_string());
        }
        out.push(entry.sym);
        reader.consume(entry.bits as usize);
    }

    if out.len() != expected_len {
        return Err(format!("huff: got {} want {}", out.len(), expected_len));
    }
    Ok(out)
}

const LZV2_WINDOW_SIZE: usize = 65536;
const LZV2_HASH_BITS: usize = 16;
const LZV2_HASH_SIZE: usize = 1 << LZV2_HASH_BITS;
const LZV2_MIN_MATCH: usize = 3;
const LZV2_MAX_MATCH: usize = 258;
const LZV2_MAX_CHAIN: usize = 256;

fn lzv2_hash(data: &[u8], pos: usize) -> u32 {
    if pos + 3 >= data.len() {
        let mut val = 0u32;
        let rem = data.len() - pos;
        if rem >= 3 {
            val = (data[pos] as u32) | ((data[pos + 1] as u32) << 8) | ((data[pos + 2] as u32) << 16);
        } else if rem == 2 {
            val = (data[pos] as u32) | ((data[pos + 1] as u32) << 8);
        } else if rem == 1 {
            val = data[pos] as u32;
        }
        return val.wrapping_mul(0x1E35A7BD) >> (32 - LZV2_HASH_BITS);
    }
    let val = u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
    val.wrapping_mul(0x1E35A7BD) >> (32 - LZV2_HASH_BITS)
}

#[inline(always)]
fn match_len_u64(data: &[u8], p: usize, i: usize, limit: usize) -> usize {
    debug_assert!(p + limit <= data.len());
    debug_assert!(i + limit <= data.len());
    let mut l = 0;
    while l + 8 <= limit {
        let a = unsafe { std::ptr::read_unaligned(data.as_ptr().add(p + l) as *const u64) };
        let b = unsafe { std::ptr::read_unaligned(data.as_ptr().add(i + l) as *const u64) };
        if a != b {
            let diff = a ^ b;
            return l + (diff.trailing_zeros() / 8) as usize;
        }
        l += 8;
    }
    while l < limit && data[p + l] == data[i + l] {
        l += 1;
    }
    l
}

fn deflate_style_encode(data: &[u8]) -> Option<Vec<u8>> {
    let n = data.len();
    if n < 128 {
        return None;
    }

    let mut head = vec![-1i32; LZV2_HASH_SIZE];
    let mut prev = vec![0i32; LZV2_WINDOW_SIZE];
    let mut symbols = Vec::with_capacity(n);
    let mut dist_codes = Vec::with_capacity(n / 4);

    let mut extra_bits = Vec::new();
    let mut extra_buf = 0u64;
    let mut extra_buf_len = 0;

    let mut pack_extra_bits = |extra: u16, n_extra: u8| {
        if n_extra > 0 {
            extra_buf = (extra_buf << n_extra) | extra as u64;
            extra_buf_len += n_extra as usize;
            while extra_buf_len >= 8 {
                extra_buf_len -= 8;
                extra_bits.push((extra_buf >> extra_buf_len) as u8);
                extra_buf &= (1 << extra_buf_len) - 1;
            }
        }
    };

    let mut i = 0;
    const GOOD_MATCH: usize = 32;
    while i < n {
        let mut best_len = 0;
        let mut best_dist = 0;
        if i + LZV2_MIN_MATCH <= n {
            let h = lzv2_hash(data, i) as usize;
            let mut pos = head[h];
            prev[i & (LZV2_WINDOW_SIZE - 1)] = pos;
            head[h] = i as i32;
            let min_pos = (i as i32) - (LZV2_WINDOW_SIZE as i32);
            let min_pos = if min_pos < 0 { 0 } else { min_pos as usize };
            let mut cl = 0;
            let mut max_chain = LZV2_MAX_CHAIN;
            while pos >= (min_pos as i32) && cl < max_chain {
                let p = pos as usize;
                let mut limit = n - i;
                if limit > LZV2_MAX_MATCH {
                    limit = LZV2_MAX_MATCH;
                }
                if best_len < limit && data[p] == data[i] && data[p + best_len] == data[i + best_len] {
                    let l = match_len_u64(data, p, i, limit);
                    if l > best_len {
                        best_len = l;
                        best_dist = i - p;
                        if l == LZV2_MAX_MATCH {
                            break;
                        }
                        if l >= 32 {
                            max_chain = LZV2_MAX_CHAIN / 8;
                        } else if l >= 8 {
                            max_chain = LZV2_MAX_CHAIN / 2;
                        }
                    }
                }
                pos = prev[p & (LZV2_WINDOW_SIZE - 1)];
                cl += 1;
            }
        }

        if best_len >= LZV2_MIN_MATCH {
            let mut skip = false;
            if best_len < GOOD_MATCH && i + 1 + LZV2_MIN_MATCH <= n && best_len < LZV2_MAX_MATCH {
                let h2 = lzv2_hash(data, i + 1) as usize;
                let mut pos2 = head[h2];
                let min_pos2 = (i as i32) + 1 - (LZV2_WINDOW_SIZE as i32);
                let min_pos2 = if min_pos2 < 0 { 0 } else { min_pos2 as usize };
                let mut cl2 = 0;
                while pos2 >= (min_pos2 as i32) && cl2 < LZV2_MAX_CHAIN / 2 {
                    let p = pos2 as usize;
                    let mut limit = n - (i + 1);
                    if limit > LZV2_MAX_MATCH {
                        limit = LZV2_MAX_MATCH;
                    }
                    let target_len = best_len + 1;
                    if target_len < limit && data[p] == data[i + 1] && data[p + target_len] == data[i + 1 + target_len] {
                        let l = match_len_u64(data, p, i + 1, limit);
                        if l > target_len {
                            best_len = l;
                            best_dist = i + 1 - p;
                            skip = true;
                            break;
                        }
                    }
                    pos2 = prev[p & (LZV2_WINDOW_SIZE - 1)];
                    cl2 += 1;
                }
            }

            if skip {
                symbols.push(data[i] as u16);
                i += 1;
                let h3 = lzv2_hash(data, i) as usize;
                prev[i & (LZV2_WINDOW_SIZE - 1)] = head[h3];
                head[h3] = i as i32;

                // Lazy-2: check if we should skip i again in favor of i+1
                let mut skip2 = false;
                if best_len < GOOD_MATCH && i + 1 + LZV2_MIN_MATCH <= n && best_len < LZV2_MAX_MATCH {
                    let h2 = lzv2_hash(data, i + 1) as usize;
                    let mut pos2 = head[h2];
                    let min_pos2 = (i as i32) + 1 - (LZV2_WINDOW_SIZE as i32);
                    let min_pos2 = if min_pos2 < 0 { 0 } else { min_pos2 as usize };
                    let mut cl2 = 0;
                    while pos2 >= (min_pos2 as i32) && cl2 < LZV2_MAX_CHAIN / 2 {
                        let p = pos2 as usize;
                        let mut limit = n - (i + 1);
                        if limit > LZV2_MAX_MATCH {
                            limit = LZV2_MAX_MATCH;
                        }
                        let target_len = best_len + 1;
                        if target_len < limit && data[p] == data[i + 1] && data[p + target_len] == data[i + 1 + target_len] {
                            let l = match_len_u64(data, p, i + 1, limit);
                            if l > target_len {
                                best_len = l;
                                best_dist = i + 1 - p;
                                skip2 = true;
                                break;
                            }
                        }
                        pos2 = prev[p & (LZV2_WINDOW_SIZE - 1)];
                        cl2 += 1;
                    }
                }

                if skip2 {
                    symbols.push(data[i] as u16);
                    i += 1;
                    let h3 = lzv2_hash(data, i) as usize;
                    prev[i & (LZV2_WINDOW_SIZE - 1)] = head[h3];
                    head[h3] = i as i32;
                }
            }

            let (len_code, len_extra, len_n_extra) = len_to_code(best_len as u16);
            let (dist_code, dist_extra, dist_n_extra) = dist_to_code((best_dist - 1) as u16);

            symbols.push(256 + len_code as u16);
            dist_codes.push(dist_code);

            pack_extra_bits(len_extra, len_n_extra);
            pack_extra_bits(dist_extra, dist_n_extra);

            for j in 1..best_len {
                if i + j + LZV2_MIN_MATCH <= n {
                    let h = lzv2_hash(data, i + j) as usize;
                    prev[(i + j) & (LZV2_WINDOW_SIZE - 1)] = head[h];
                    head[h] = (i + j) as i32;
                }
            }
            i += best_len;
        } else {
            symbols.push(data[i] as u16);
            i += 1;
        }
    }

    if extra_buf_len > 0 {
        extra_bits.push((extra_buf << (8 - extra_buf_len)) as u8);
    }

    let sym_comp = huff_encode_uint16(&symbols)?;
    let mut dist_comp = Vec::new();
    if !dist_codes.is_empty() {
        if let Some(comp) = huff_encode(&dist_codes) {
            dist_comp = comp;
        } else {
            dist_comp = dist_codes.clone();
        }
    }

    let out_len = 20 + sym_comp.len() + dist_comp.len() + extra_bits.len();
    if out_len >= n {
        return None;
    }

    let mut out = Vec::with_capacity(out_len);
    out.extend_from_slice(&(sym_comp.len() as u32).to_le_bytes());
    out.extend_from_slice(&(symbols.len() as u32).to_le_bytes());
    out.extend_from_slice(&(dist_codes.len() as u32).to_le_bytes()); // num_dist
    out.extend_from_slice(&(dist_comp.len() as u32).to_le_bytes());
    out.extend_from_slice(&(extra_bits.len() as u32).to_le_bytes());

    out.extend_from_slice(&sym_comp);
    out.extend_from_slice(&dist_comp);
    out.extend_from_slice(&extra_bits);

    Some(out)
}

fn deflate_style_decode(data: &[u8], expected_len: usize) -> Result<Vec<u8>, String> {
    if data.len() < 20 {
        return Err("deflateStyle: too short".to_string());
    }

    let sym_comp_sz = u32::from_le_bytes(data[0..4].try_into().unwrap()) as usize;
    let num_syms = u32::from_le_bytes(data[4..8].try_into().unwrap()) as usize;
    let num_dist = u32::from_le_bytes(data[8..12].try_into().unwrap()) as usize;
    let dist_comp_sz = u32::from_le_bytes(data[12..16].try_into().unwrap()) as usize;
    let extra_bits_len = u32::from_le_bytes(data[16..20].try_into().unwrap()) as usize;

    if 20 + sym_comp_sz > data.len() {
        return Err("deflateStyle: sym overrun".to_string());
    }

    let sym_cd = &data[20..20 + sym_comp_sz];
    let symbols = huff_decode_uint16(sym_cd, num_syms)?;

    let dist_start = 20 + sym_comp_sz;
    if dist_start + dist_comp_sz > data.len() {
        return Err("deflateStyle: dist overrun".to_string());
    }
    let dist_cd = &data[dist_start..dist_start + dist_comp_sz];
    let dist_code_bytes = if num_dist > 0 {
        if dist_comp_sz == num_dist {
            dist_cd.to_vec()
        } else {
            huff_decode(dist_cd, num_dist)?
        }
    } else {
        Vec::new()
    };

    let eb_start = dist_start + dist_comp_sz;
    if eb_start + extra_bits_len > data.len() {
        return Err("deflateStyle: extra bits overrun".to_string());
    }
    let eb = &data[eb_start..eb_start + extra_bits_len];

    let mut eb_buf = 0u64;
    let mut eb_buf_len = 0;
    let mut eb_idx = 0;

    let mut read_extra_bits = |n_extra: u8| -> Result<u16, String> {
        if n_extra == 0 {
            return Ok(0);
        }
        let n = n_extra as usize;
        while eb_buf_len < n && eb_idx < eb.len() {
            eb_buf = (eb_buf << 8) | (eb[eb_idx] as u64);
            eb_buf_len += 8;
            eb_idx += 1;
        }
        if eb_buf_len < n {
            return Err("deflateStyle: extra bits underrun".to_string());
        }
        eb_buf_len -= n;
        let val = ((eb_buf >> eb_buf_len) & ((1 << n) - 1)) as u16;
        eb_buf &= (1 << eb_buf_len) - 1;
        Ok(val)
    };

    let mut out = Vec::with_capacity(expected_len);
    let mut di = 0;

    for si in 0..num_syms {
        let sym = symbols[si];
        if sym < 256 {
            out.push(sym as u8);
        } else {
            let len_code = (sym - 256) as u8;
            if len_code > 28 {
                return Err(format!("deflateStyle: invalid length code {}", len_code));
            }
            let n_extra_len = LEN_CODE_TABLE[len_code as usize].1;
            let extra_len = read_extra_bits(n_extra_len)?;
            let ml = code_to_len(len_code, extra_len) as usize;

            if di >= num_dist {
                return Err("deflateStyle: dist underrun".to_string());
            }
            let dist_code = dist_code_bytes[di];
            di += 1;
            if dist_code >= 32 {
                return Err(format!("deflateStyle: invalid dist code {}", dist_code));
            }
            let n_extra_dist = DIST_CODE_TABLE[dist_code as usize].1;
            let extra_dist = read_extra_bits(n_extra_dist)?;
            let dist = (code_to_dist(dist_code, extra_dist) as usize) + 1;

            if dist > out.len() {
                return Err(format!("deflateStyle: bad dist {}", dist));
            }
            if dist >= ml {
                let start = out.len() - dist;
                out.extend_from_within(start..start + ml);
            } else if dist == 1 {
                let last = *out.last().unwrap();
                out.resize(out.len() + ml, last);
            } else {
                let mut copied = 0;
                while copied < ml {
                    let chunk = std::cmp::min(ml - copied, dist);
                    let start = out.len() - dist;
                    out.extend_from_within(start..start + chunk);
                    copied += chunk;
                }
            }
        }
    }

    if out.len() != expected_len {
        return Err(format!("deflateStyle: got {} want {}", out.len(), expected_len));
    }
    Ok(out)
}

// ══════════════════════════════════════════════════════════════
// Phase C: Blocked deflate — per-block Huffman trees
// Each 32KB block gets its own optimal Huffman tree
// Format: [4 numBlocks] { [4 compSize] [4 origSize] [blockData] }*
// ══════════════════════════════════════════════════════════════

const BLOCK_SIZE: usize = 32 * 1024; // 32KB blocks

// Number of worker threads to use for a given amount of work.
fn num_threads(work: usize) -> usize {
    if work <= 1 {
        return 1;
    }
    std::thread::available_parallelism()
        .map(|x| x.get())
        .unwrap_or(4)
        .min(work)
        .max(1)
}

// Parallel map: applies `f` to each index 0..n, preserving order.
// Uses scoped threads (no external dependencies). Falls back to serial for tiny work.
fn parallel_map<T, F>(n: usize, f: F) -> Vec<T>
where
    T: Send,
    F: Fn(usize) -> T + Sync,
{
    let threads = num_threads(n);
    if threads <= 1 {
        return (0..n).map(|i| f(i)).collect();
    }

    use std::mem::MaybeUninit;
    let mut results: Vec<MaybeUninit<T>> = (0..n).map(|_| MaybeUninit::uninit()).collect();
    let chunk_size = (n + threads - 1) / threads;
    let f = &f;
    std::thread::scope(|s| {
        let mut base = 0usize;
        for chunk in results.chunks_mut(chunk_size) {
            let start = base;
            base += chunk.len();
            s.spawn(move || {
                for (j, slot) in chunk.iter_mut().enumerate() {
                    slot.write(f(start + j));
                }
            });
        }
    });

    unsafe {
        let mut me = std::mem::ManuallyDrop::new(results);
        Vec::from_raw_parts(
            me.as_mut_ptr() as *mut T,
            me.len(),
            me.capacity(),
        )
    }
}

fn deflate_blocked_encode(data: &[u8]) -> Option<Vec<u8>> {
    let n = data.len();
    if n < 128 {
        return None;
    }

    let num_blocks = (n + BLOCK_SIZE - 1) / BLOCK_SIZE;

    // Compress each block independently and in parallel; each block gets its own
    // optimal Huffman tree. Output bytes per block include the 9-byte header.
    let encoded_blocks: Vec<Vec<u8>> = parallel_map(num_blocks, |b| {
        let start = b * BLOCK_SIZE;
        let end = std::cmp::min(start + BLOCK_SIZE, n);
        let block = &data[start..end];

        match deflate_style_encode(block) {
            Some(c) if c.len() < block.len() => {
                let mut v = Vec::with_capacity(c.len() + 9);
                v.extend_from_slice(&(c.len() as u32).to_le_bytes());
                v.extend_from_slice(&(block.len() as u32).to_le_bytes());
                v.push(1); // compressed flag
                v.extend_from_slice(&c);
                v
            }
            _ => {
                let mut v = Vec::with_capacity(block.len() + 9);
                v.extend_from_slice(&(block.len() as u32).to_le_bytes());
                v.extend_from_slice(&(block.len() as u32).to_le_bytes());
                v.push(0); // raw flag
                v.extend_from_slice(block);
                v
            }
        }
    });

    let total: usize = 4 + encoded_blocks.iter().map(|b| b.len()).sum::<usize>();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&(num_blocks as u32).to_le_bytes());
    for b in &encoded_blocks {
        out.extend_from_slice(b);
    }

    if out.len() >= n {
        return None;
    }
    Some(out)
}

fn deflate_blocked_decode(data: &[u8], expected_len: usize) -> Result<Vec<u8>, String> {
    if data.len() < 4 {
        return Err("blocked: too short".to_string());
    }
    let num_blocks = u32::from_le_bytes(data[0..4].try_into().unwrap()) as usize;
    let mut pos = 4;

    // Pass 1 (serial, cheap): parse block descriptors and compute output offsets.
    // Each entry: (output_offset, compressed_slice, orig_size, flag)
    let mut descs: Vec<(usize, &[u8], usize, u8)> = Vec::with_capacity(num_blocks);
    let mut out_off = 0usize;
    for i in 0..num_blocks {
        if pos + 9 > data.len() {
            return Err(format!("blocked: block {} header overrun", i));
        }
        let comp_size = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
        let orig_size = u32::from_le_bytes(data[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let flag = data[pos + 8];
        pos += 9;

        if pos + comp_size > data.len() {
            return Err(format!("blocked: block {} data overrun", i));
        }
        let block_data = &data[pos..pos + comp_size];
        pos += comp_size;

        if out_off + orig_size > expected_len {
            return Err("blocked: output overrun".to_string());
        }
        descs.push((out_off, block_data, orig_size, flag));
        out_off += orig_size;
    }

    if out_off != expected_len {
        return Err(format!("blocked: got {} want {}", out_off, expected_len));
    }

    // Pass 2 (parallel): decode each block into its own disjoint output slice.
    let mut out = vec![0u8; expected_len];

    // Carve `out` into disjoint mutable slices, one per block, in order.
    let mut work: Vec<(&mut [u8], &[u8], usize, u8)> = Vec::with_capacity(descs.len());
    let mut remaining: &mut [u8] = &mut out[..];
    for &(_, comp, orig, flag) in &descs {
        let (head, tail) = remaining.split_at_mut(orig);
        work.push((head, comp, orig, flag));
        remaining = tail;
    }

    let threads = num_threads(work.len());
    if threads <= 1 {
        for (slot, comp, orig, flag) in work.iter_mut() {
            decode_block_into(slot, comp, *orig, *flag)?;
        }
    } else {
        let has_error = std::sync::atomic::AtomicBool::new(false);
        let chunk_size = (work.len() + threads - 1) / threads;
        let has_error_ref = &has_error;
        let results = std::thread::scope(|s| {
            let mut handles = Vec::new();
            for chunk in work.chunks_mut(chunk_size) {
                let handle = s.spawn(move || -> Result<(), String> {
                    for (slot, comp, orig, flag) in chunk.iter_mut() {
                        if has_error_ref.load(std::sync::atomic::Ordering::Relaxed) {
                            return Ok(());
                        }
                        if let Err(e) = decode_block_into(slot, comp, *orig, *flag) {
                            has_error_ref.store(true, std::sync::atomic::Ordering::Relaxed);
                            return Err(e);
                        }
                    }
                    Ok(())
                });
                handles.push(handle);
            }
            handles.into_iter().map(|h| h.join()).collect::<Vec<_>>()
        });

        for res in results {
            match res {
                Ok(Err(e)) => return Err(e),
                Err(any) => return Err(format!("Thread panicked: {:?}", any)),
                _ => {}
            }
        }
    }

    Ok(out)
}

#[inline]
fn decode_block_into(slot: &mut [u8], comp: &[u8], orig: usize, flag: u8) -> Result<(), String> {
    if flag == 0 {
        if comp.len() != orig {
            return Err("blocked: raw block size mismatch".to_string());
        }
        slot.copy_from_slice(comp);
    } else {
        let decoded = deflate_style_decode(comp, orig)?;
        slot.copy_from_slice(&decoded);
    }
    Ok(())
}

// ══════════════════════════════════════════════════════════════
// Phase D: Byte Shuffle (stride=4 for float32 weights)
// Groups bytes by position within stride: all byte[0], all byte[1], ...
// This exposes structural redundancy in IEEE754 floats
// ══════════════════════════════════════════════════════════════

fn byte_shuffle(data: &[u8], stride: usize) -> Vec<u8> {
    if stride == 2 {
        let n = data.len();
        let groups = n / 2;
        let mut out = vec![0u8; n];
        
        // s = 0
        {
            let lane = &mut out[0..groups];
            for g in 0..groups {
                lane[g] = data[g * 2];
            }
            let mut prev = 0u8;
            for val in lane.iter_mut() {
                let curr = *val;
                *val = curr.wrapping_sub(prev);
                prev = curr;
            }
        }
        
        // s = 1
        {
            let lane = &mut out[groups..2 * groups];
            for g in 0..groups {
                lane[g] = data[g * 2 + 1];
            }
            let mut prev = 0u8;
            for val in lane.iter_mut() {
                let curr = *val;
                *val = curr.wrapping_sub(prev);
                prev = curr;
            }
        }
        
        if n > groups * 2 {
            out[groups * 2..n].copy_from_slice(&data[groups * 2..n]);
        }
        return out;
    }
    
    if stride == 4 {
        let n = data.len();
        let groups = n / 4;
        let mut out = vec![0u8; n];
        
        for s in 0..4 {
            let lane = &mut out[s * groups..(s + 1) * groups];
            for g in 0..groups {
                lane[g] = data[g * 4 + s];
            }
            let mut prev = 0u8;
            for val in lane.iter_mut() {
                let curr = *val;
                *val = curr.wrapping_sub(prev);
                prev = curr;
            }
        }
        
        if n > groups * 4 {
            out[groups * 4..n].copy_from_slice(&data[groups * 4..n]);
        }
        return out;
    }

    let n = data.len();
    let groups = n / stride;
    let mut out = vec![0u8; n];
    for s in 0..stride {
        let lane = &mut out[s * groups..(s + 1) * groups];
        for (g, dst) in lane.iter_mut().enumerate() {
            *dst = data[g * stride + s];
        }
        // Apply delta encoding in-place
        let mut prev = 0u8;
        for val in lane.iter_mut() {
            let curr = *val;
            *val = curr.wrapping_sub(prev);
            prev = curr;
        }
    }
    // Remainder bytes
    if n > groups * stride {
        out[groups * stride..n].copy_from_slice(&data[groups * stride..n]);
    }
    out
}

fn byte_unshuffle(data: &[u8], stride: usize) -> Vec<u8> {
    if stride == 2 {
        let n = data.len();
        let groups = n / 2;
        let mut out = vec![0u8; n];
        
        let lane0 = &data[0..groups];
        let mut accum0 = 0u8;
        for g in 0..groups {
            accum0 = accum0.wrapping_add(lane0[g]);
            out[g * 2] = accum0;
        }
        
        let lane1 = &data[groups..2 * groups];
        let mut accum1 = 0u8;
        for g in 0..groups {
            accum1 = accum1.wrapping_add(lane1[g]);
            out[g * 2 + 1] = accum1;
        }
        
        let base = groups * 2;
        if n > base {
            out[base..n].copy_from_slice(&data[base..n]);
        }
        return out;
    }
    
    if stride == 4 {
        let n = data.len();
        let groups = n / 4;
        let mut out = vec![0u8; n];
        
        for s in 0..4 {
            let lane = &data[s * groups..(s + 1) * groups];
            let mut accum = 0u8;
            for g in 0..groups {
                accum = accum.wrapping_add(lane[g]);
                out[g * 4 + s] = accum;
            }
        }
        
        let base = groups * 4;
        if n > base {
            out[base..n].copy_from_slice(&data[base..n]);
        }
        return out;
    }

    let n = data.len();
    let groups = n / stride;
    let mut out = vec![0u8; n];
    for s in 0..stride {
        let lane = &data[s * groups..(s + 1) * groups];
        let mut accum = 0u8;
        for (g, &val) in lane.iter().enumerate() {
            accum = accum.wrapping_add(val);
            out[g * stride + s] = accum;
        }
    }
    // Remainder bytes
    let base = groups * stride;
    if n > base {
        out[base..n].copy_from_slice(&data[base..n]);
    }
    out
}

// ══════════════════════════════════════════════════════════════
// Smart compress: try plain, shuffle, blocked, shuffle+blocked
// Pick the best ratio automatically
// ══════════════════════════════════════════════════════════════

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum CompressMethod {
    Plain,       // 0
    Blocked,     // 1
    Shuffle,     // 2  stride=4 + plain deflate
    ShuffleBlk,  // 3  stride=4 + blocked deflate
    Shuffle2,    // 4  stride=2 + plain deflate
    Shuffle2Blk, // 5  stride=2 + blocked deflate
}

impl CompressMethod {
    /// Stable on-disk identifier for the method.
    pub fn to_u8(self) -> u8 {
        match self {
            CompressMethod::Plain => 0,
            CompressMethod::Blocked => 1,
            CompressMethod::Shuffle => 2,
            CompressMethod::ShuffleBlk => 3,
            CompressMethod::Shuffle2 => 4,
            CompressMethod::Shuffle2Blk => 5,
        }
    }

    /// Parse a method from its on-disk identifier.
    pub fn from_u8(v: u8) -> Option<CompressMethod> {
        match v {
            0 => Some(CompressMethod::Plain),
            1 => Some(CompressMethod::Blocked),
            2 => Some(CompressMethod::Shuffle),
            3 => Some(CompressMethod::ShuffleBlk),
            4 => Some(CompressMethod::Shuffle2),
            5 => Some(CompressMethod::Shuffle2Blk),
            _ => None,
        }
    }
}

pub fn smart_compress(data: &[u8]) -> Option<(Vec<u8>, CompressMethod)> {
    let n = data.len();
    if n < 128 {
        return None;
    }

    let mut best: Option<(Vec<u8>, CompressMethod)> = None;
    let consider = |cand: Option<Vec<u8>>, m: CompressMethod, best: &mut Option<(Vec<u8>, CompressMethod)>| {
        if let Some(c) = cand {
            if best.is_none() || c.len() < best.as_ref().unwrap().0.len() {
                *best = Some((c, m));
            }
        }
    };

    // Above this size, the single-threaded non-blocked passes become the bottleneck
    // while being ratio-equivalent to their blocked counterparts, so we skip them.
    const PARALLEL_ONLY_THRESHOLD: usize = 1 << 20; // 1 MB

    if n >= PARALLEL_ONLY_THRESHOLD {
        // Large files: only internally-parallel blocked variants (fast + per-block trees).
        consider(deflate_blocked_encode(data), CompressMethod::Blocked, &mut best);

        let shuffled4 = byte_shuffle(data, 4);
        consider(deflate_blocked_encode(&shuffled4), CompressMethod::ShuffleBlk, &mut best);
        drop(shuffled4);

        let shuffled2 = byte_shuffle(data, 2);
        consider(deflate_blocked_encode(&shuffled2), CompressMethod::Shuffle2Blk, &mut best);
    } else {
        // Small/medium files: cheap single-threaded passes over candidate transforms,
        // plus blocked variants when the data spans more than one block.
        consider(deflate_style_encode(data), CompressMethod::Plain, &mut best);

        let shuffled4 = byte_shuffle(data, 4);
        consider(deflate_style_encode(&shuffled4), CompressMethod::Shuffle, &mut best);

        let shuffled2 = byte_shuffle(data, 2);
        consider(deflate_style_encode(&shuffled2), CompressMethod::Shuffle2, &mut best);

        if n >= BLOCK_SIZE {
            consider(deflate_blocked_encode(data), CompressMethod::Blocked, &mut best);
            consider(deflate_blocked_encode(&shuffled4), CompressMethod::ShuffleBlk, &mut best);
            consider(deflate_blocked_encode(&shuffled2), CompressMethod::Shuffle2Blk, &mut best);
        }
    }

    best
}

pub fn smart_decompress(data: &[u8], method: CompressMethod, expected_len: usize) -> Result<Vec<u8>, String> {
    match method {
        CompressMethod::Plain => deflate_style_decode(data, expected_len),
        CompressMethod::Blocked => deflate_blocked_decode(data, expected_len),
        CompressMethod::Shuffle => {
            let decoded = deflate_style_decode(data, expected_len)?;
            Ok(byte_unshuffle(&decoded, 4))
        }
        CompressMethod::ShuffleBlk => {
            let decoded = deflate_blocked_decode(data, expected_len)?;
            Ok(byte_unshuffle(&decoded, 4))
        }
        CompressMethod::Shuffle2 => {
            let decoded = deflate_style_decode(data, expected_len)?;
            Ok(byte_unshuffle(&decoded, 2))
        }
        CompressMethod::Shuffle2Blk => {
            let decoded = deflate_blocked_decode(data, expected_len)?;
            Ok(byte_unshuffle(&decoded, 2))
        }
    }
}

pub fn method_name(m: CompressMethod) -> &'static str {
    match m {
        CompressMethod::Plain => "plain",
        CompressMethod::Blocked => "blocked",
        CompressMethod::Shuffle => "shuf+defl",
        CompressMethod::ShuffleBlk => "shuf+blk",
        CompressMethod::Shuffle2 => "shuf2+defl",
        CompressMethod::Shuffle2Blk => "shuf2+blk",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(data: &[u8]) {
        match smart_compress(data) {
            Some((comp, method)) => {
                let back = smart_decompress(&comp, method, data.len()).unwrap();
                assert_eq!(back, data, "roundtrip mismatch via {}", method_name(method));
            }
            None => {
                // Too small or incompressible — nothing to verify.
            }
        }
    }

    #[test]
    fn roundtrip_text() {
        let mut s = String::new();
        for i in 0..4000 {
            s.push_str(&format!("line {} the quick brown fox jumps over\n", i % 97));
        }
        roundtrip(s.as_bytes());
    }

    #[test]
    fn roundtrip_repeated() {
        let data = vec![0xABu8; 100_000];
        roundtrip(&data);
    }

    #[test]
    fn roundtrip_method_ids_stable() {
        for m in [
            CompressMethod::Plain,
            CompressMethod::Blocked,
            CompressMethod::Shuffle,
            CompressMethod::ShuffleBlk,
            CompressMethod::Shuffle2,
            CompressMethod::Shuffle2Blk,
        ] {
            assert_eq!(CompressMethod::from_u8(m.to_u8()), Some(m));
        }
        assert_eq!(CompressMethod::from_u8(200), None);
    }

    #[test]
    fn roundtrip_f32_weights() {
        let weights: Vec<f32> = (0..10_000).map(|i| (i as f32) * 0.001).collect();
        let bytes = unsafe { std::slice::from_raw_parts(weights.as_ptr() as *const u8, weights.len() * 4) };
        roundtrip(bytes);
    }
}
