// Big Bounce codec: LZ77 + per-block Huffman with byte-shuffle transforms.
// Self-contained, zero external dependencies. Extracted from the reference
// benchmark implementation and exposed as a reusable library module.
#![allow(dead_code)]
#![allow(unused_assignments)]
#![allow(clippy::needless_range_loop)]
#![allow(clippy::type_complexity)]

use std::io::{self, Read, Seek};


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

fn dist_to_code(dist: u16) -> (u8, u16, u8) {
    if dist < 4 {
        return (dist as u8, 0, 0);
    }
    let msb = 15 - dist.leading_zeros() as u8;
    let extra_bits = msb - 1;
    let code = (msb << 1) | ((dist >> extra_bits) as u8 & 1);
    (code, dist & ((1 << extra_bits) - 1), extra_bits)
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

#[derive(Clone, Copy, Default)]
struct FlatNode {
    freq: usize,
    sym: i16,
    left: Option<u16>,
    right: Option<u16>,
}

fn huff_assign_lengths(
    max_bits: usize,
    alphabet_size: usize,
    freq: &[usize],
    code_lens: &mut [u8],
) {
    for i in 0..alphabet_size {
        code_lens[i] = 0;
    }

    let mut unique_count = 0;
    for &f in freq {
        if f > 0 {
            unique_count += 1;
        }
    }

    if unique_count == 0 {
        return;
    }

    if unique_count == 1 {
        for i in 0..alphabet_size {
            if freq[i] > 0 {
                code_lens[i] = 1;
                break;
            }
        }
        return;
    }

    let mut nodes = [FlatNode::default(); 576];
    let mut nodes_len = 0;

    let mut leaves = [(0usize, 0u16); 288];
    let mut num_leaves = 0;

    for sym in 0..alphabet_size {
        if freq[sym] > 0 {
            leaves[num_leaves] = (freq[sym], sym as u16);
            num_leaves += 1;
        }
    }

    if num_leaves == 0 {
        return;
    }
    if num_leaves == 1 {
        code_lens[leaves[0].1 as usize] = 1;
        return;
    }

    leaves[..num_leaves].sort_unstable_by_key(|k| k.0);

    for i in 0..num_leaves {
        let (f, sym) = leaves[i];
        let idx = nodes_len;
        nodes[idx] = FlatNode {
            freq: f,
            sym: sym as i16,
            left: None,
            right: None,
        };
        leaves[i] = (f, idx as u16);
        nodes_len += 1;
    }

    let mut q1_head = 0;
    let mut q2_head = 0;
    let mut q2_tail = 0;
    let mut q2 = [(0usize, 0u16); 288];

    while (num_leaves - q1_head) + (q2_tail - q2_head) > 1 {
        let mut get_min = || {
            if q1_head < num_leaves {
                if q2_head < q2_tail {
                    if leaves[q1_head].0 <= q2[q2_head].0 {
                        let res = leaves[q1_head];
                        q1_head += 1;
                        res
                    } else {
                        let res = q2[q2_head];
                        q2_head += 1;
                        res
                    }
                } else {
                    let res = leaves[q1_head];
                    q1_head += 1;
                    res
                }
            } else {
                let res = q2[q2_head];
                q2_head += 1;
                res
            }
        };

        let (f1, child1) = get_min();
        let (f2, child2) = get_min();

        let merged_freq = f1 + f2;
        let merged_idx = nodes_len;
        nodes[merged_idx] = FlatNode {
            freq: merged_freq,
            sym: -1,
            left: Some(child1),
            right: Some(child2),
        };
        nodes_len += 1;
        q2[q2_tail] = (merged_freq, merged_idx as u16);
        q2_tail += 1;
    }

    let root = if q1_head < num_leaves { leaves[q1_head].1 } else { q2[q2_head].1 } as usize;

    let mut stack = [(0u16, 0u8); 576];
    let mut stack_len = 0;
    stack[stack_len] = (root as u16, 0);
    stack_len += 1;

    while stack_len > 0 {
        stack_len -= 1;
        let (curr_idx, depth) = stack[stack_len];
        let node = &nodes[curr_idx as usize];
        if node.sym >= 0 {
            code_lens[node.sym as usize] = depth;
        } else {
            if let Some(right) = node.right {
                stack[stack_len] = (right, depth + 1);
                stack_len += 1;
            }
            if let Some(left) = node.left {
                stack[stack_len] = (left, depth + 1);
                stack_len += 1;
            }
        }
    }

    let mut bl_count = [0usize; 288];
    let mut max_len = 0;
    for &len in code_lens.iter() {
        if len > 0 {
            let l = len as usize;
            bl_count[l] += 1;
            if l > max_len {
                max_len = l;
            }
        }
    }

    if max_len <= max_bits {
        return;
    }

    let mut sum_scaled = 0u128;
    for d in (max_bits + 1)..=max_len {
        if bl_count[d] > 0 {
            let count = bl_count[d] as u128;
            let term1 = count << 97;
            let term2 = count << (112 - d);
            sum_scaled += term1 - term2;
            bl_count[max_bits] += bl_count[d];
            bl_count[d] = 0;
        }
    }

    let mut overflow = (sum_scaled >> 96) as usize;

    while overflow > 0 {
        let mut bits = max_bits - 1;
        while bits > 0 && bl_count[bits] == 0 {
            bits -= 1;
        }
        if bits == 0 {
            break;
        }
        bl_count[bits] -= 1;
        bl_count[bits + 1] += 2;
        bl_count[max_bits] -= 1;
        overflow -= 2;
    }

    let mut active_syms = [0u16; 288];
    let mut active_count = 0;
    for sym in 0..alphabet_size {
        if freq[sym] > 0 {
            active_syms[active_count] = sym as u16;
            active_count += 1;
        }
    }

    active_syms[..active_count].sort_unstable_by_key(|&sym| freq[sym as usize]);

    let mut sym_idx = 0;
    for bits in (1..=max_bits).rev() {
        let count = bl_count[bits];
        for _ in 0..count {
            if sym_idx < active_count {
                code_lens[active_syms[sym_idx] as usize] = bits as u8;
                sym_idx += 1;
            }
        }
    }
}

#[inline(always)]
fn pack_table_entry(sym: u16, bits: u8) -> u16 {
    ((bits as u16) << 9) | sym
}

#[inline(always)]
fn unpack_table_entry(entry: u16) -> (u16, u8) {
    (entry & 0x1FF, (entry >> 9) as u8)
}

const SUB_TABLE_INDICATOR: u8 = 127;

struct BitWriter {
    out: Vec<u8>,
    bit_buf: u64,
    bit_pos: usize,
}

impl BitWriter {
    #[inline(always)]
    fn new() -> Self {
        Self {
            out: Vec::new(),
            bit_buf: 0,
            bit_pos: 0,
        }
    }

    #[inline(always)]
    fn from_vec(out: Vec<u8>) -> Self {
        Self {
            out,
            bit_buf: 0,
            bit_pos: 0,
        }
    }

    #[inline(always)]
    fn write(&mut self, val: u32, n: usize) {
        self.bit_buf = (self.bit_buf << n) | (val as u64 & ((1 << n) - 1));
        self.bit_pos += n;
        if self.bit_pos >= 32 {
            self.bit_pos -= 32;
            let chunk = (self.bit_buf >> self.bit_pos) as u32;
            self.out.extend_from_slice(&chunk.to_be_bytes());
        }
    }

    #[inline(always)]
    fn finish(mut self) -> Vec<u8> {
        while self.bit_pos >= 8 {
            self.bit_pos -= 8;
            self.out.push((self.bit_buf >> self.bit_pos) as u8);
        }
        if self.bit_pos > 0 {
            self.out.push((self.bit_buf << (8 - self.bit_pos)) as u8);
        }
        self.out
    }
}

fn encode_code_lens(lens: &[u8]) -> Vec<u8> {
    let mut writer = BitWriter::new();

    let mut i = 0;
    let mut prev = None;
    while i < lens.len() {
        let val = lens[i];
        if val == 0 {
            let mut run = 0;
            while i + run < lens.len() && lens[i + run] == 0 {
                run += 1;
            }
            i += run;
            let mut remaining = run;
            while remaining > 0 {
                if remaining >= 11 {
                    let count = std::cmp::min(remaining, 138);
                    writer.write(18, 5);
                    writer.write((count - 11) as u32, 7);
                    remaining -= count;
                } else if remaining >= 3 {
                    let count = std::cmp::min(remaining, 10);
                    writer.write(17, 5);
                    writer.write((count - 3) as u32, 3);
                    remaining -= count;
                } else {
                    writer.write(0, 5);
                    remaining -= 1;
                }
            }
            prev = Some(0);
        } else {
            let mut run = 1;
            while i + run < lens.len() && lens[i + run] == val {
                run += 1;
            }
            if prev == Some(val) && run >= 3 {
                let count = std::cmp::min(run, 6);
                writer.write(16, 5);
                writer.write((count - 3) as u32, 2);
                i += count;
            } else {
                writer.write(val as u32, 5);
                prev = Some(val);
                i += 1;
            }
        }
    }

    writer.finish()
}

fn decode_code_lens(encoded: &[u8], out: &mut [u8]) -> Result<(), String> {
    let mut reader = BitReader::new(encoded);
    let mut i = 0;
    let mut prev = None;

    while i < out.len() {
        reader.fill();
        let sym = reader.peek(5);
        reader.consume(5);

        if sym <= 15 {
            out[i] = sym as u8;
            prev = Some(sym as u8);
            i += 1;
        } else if sym == 16 {
            let extra = reader.peek(2);
            reader.consume(2);
            let count = 3 + extra as usize;
            let p = match prev {
                Some(p) => p,
                None => return Err("RLE decode: code 16 repeat with no previous value".to_string()),
            };
            if i + count > out.len() {
                return Err("RLE decode: repeat exceeds length".to_string());
            }
            for _ in 0..count {
                out[i] = p;
                i += 1;
            }
        } else if sym == 17 {
            let extra = reader.peek(3);
            reader.consume(3);
            let count = 3 + extra as usize;
            if i + count > out.len() {
                return Err("RLE decode: repeat exceeds length".to_string());
            }
            for _ in 0..count {
                out[i] = 0;
                i += 1;
            }
            prev = Some(0);
        } else if sym == 18 {
            let extra = reader.peek(7);
            reader.consume(7);
            let count = 11 + extra as usize;
            if i + count > out.len() {
                return Err("RLE decode: repeat exceeds length".to_string());
            }
            for _ in 0..count {
                out[i] = 0;
                i += 1;
            }
            prev = Some(0);
        } else {
            return Err(format!("RLE decode: invalid symbol {}", sym));
        }
    }
    Ok(())
}

const HUFF_UINT16_ALPHABET_SIZE: usize = 286;

fn huff_encode_uint16(data: &[u16], version: u8) -> Option<Vec<u8>> {
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
    huff_assign_lengths(15, HUFF_UINT16_ALPHABET_SIZE, &freq, &mut code_lens);

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

    let rle_data = encode_code_lens(&code_lens);
    let mut out = Vec::with_capacity(data.len() + 3 + rle_data.len());
    out.push(max_bits as u8);
    if version >= 2 {
        let len_bytes = (rle_data.len() as u16).to_le_bytes();
        out.push(len_bytes[0]);
        out.push(len_bytes[1]);
    } else {
        out.push(rle_data.len() as u8);
    }
    out.extend_from_slice(&rle_data);

    let mut bw = BitWriter::from_vec(out);
    for &b in data {
        let cl = code_widths[b as usize] as usize;
        let c = codes[b as usize];
        bw.write(c, cl);
    }

    let out = bw.finish();

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
        if self.idx + 7 < self.data.len() {
            let bytes_to_take = (63 - self.bits_left) / 8;
            if bytes_to_take > 0 {
                let v = u64::from_be_bytes(self.data[self.idx..self.idx + 8].try_into().unwrap());
                let shift = 64 - bytes_to_take * 8;
                self.bit_buf = (self.bit_buf << (bytes_to_take * 8)) | (v >> shift);
                self.bits_left += bytes_to_take * 8;
                self.idx += bytes_to_take;
            }
        } else {
            self.fill_slow();
        }
    }

    #[cold]
    fn fill_slow(&mut self) {
        let bytes_to_take = (64 - self.bits_left) / 8;
        let available = self.data.len() - self.idx;
        let to_read = std::cmp::min(bytes_to_take, available);
        for _ in 0..to_read {
            self.bit_buf = (self.bit_buf << 8) | (self.data[self.idx] as u64);
            self.bits_left += 8;
            self.idx += 1;
        }
    }

    #[inline(always)]
    fn peek(&self, n: usize) -> u32 {
        if self.bits_left >= n {
            let shift_amt = self.bits_left - n;
            let mask = (1u32 << n) - 1;
            ((self.bit_buf >> shift_amt) as u32) & mask
        } else {
            let mask = (1u64 << self.bits_left) - 1;
            ((self.bit_buf & mask) << (n - self.bits_left)) as u32
        }
    }

    #[inline(always)]
    fn consume(&mut self, n: usize) {
        self.bits_left = self.bits_left.saturating_sub(n);
    }
}

fn build_huff16_tables_and_stream(data: &[u8], version: u8) -> Result<([u16; 2048], Vec<u16>, &[u8]), String> {
    let (max_bits, _rle_len, header_len, start_idx) = if version >= 2 {
        if data.len() < 3 {
            return Err("huffUint16: data too short for header".to_string());
        }
        let max_bits = data[0] as usize;
        let rle_len = u16::from_le_bytes([data[1], data[2]]) as usize;
        (max_bits, rle_len, 3 + rle_len, 3)
    } else {
        if data.len() < 2 {
            return Err("huffUint16: data too short for header".to_string());
        }
        let max_bits = data[0] as usize;
        let rle_len = data[1] as usize;
        (max_bits, rle_len, 2 + rle_len, 2)
    };

    if max_bits > 15 || max_bits == 0 {
        return Err(format!("huffUint16: invalid maxBits {}", max_bits));
    }

    if data.len() < header_len {
        return Err("huffUint16: data too short for RLE header".to_string());
    }

    let mut code_lens = [0u8; HUFF_UINT16_ALPHABET_SIZE];
    decode_code_lens(&data[start_idx..header_len], &mut code_lens)?;
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

    let mut root_table = [0u16; 2048];
    let mut sub_tables = Vec::new();
    let mut sub_table_map = [-1i16; 2048];

    for sym in 0..HUFF_UINT16_ALPHABET_SIZE {
        let cl = code_lens[sym] as usize;
        if cl == 0 {
            continue;
        }
        let c = next_code[cl];
        next_code[cl] += 1;

        if cl <= 11 {
            let pad = 11 - cl;
            let start = (c << pad) as usize;
            let end = start + (1 << pad);
            for idx in start..end {
                root_table[idx] = pack_table_entry(sym as u16, cl as u8);
            }
        } else {
            let prefix = (c >> (cl - 11)) as usize;
            let sub_idx = if sub_table_map[prefix] == -1 {
                let idx = sub_tables.len() / 16;
                sub_tables.resize(sub_tables.len() + 16, 0u16);
                sub_table_map[prefix] = idx as i16;
                root_table[prefix] = pack_table_entry(idx as u16, SUB_TABLE_INDICATOR);
                idx
            } else {
                sub_table_map[prefix] as usize
            };

            let suffix = c & ((1 << (cl - 11)) - 1);
            let suffix_pad = 15 - cl;
            let start = (suffix << suffix_pad) as usize;
            let end = start + (1 << suffix_pad);
            let base_idx = sub_idx * 16;
            for idx in start..end {
                sub_tables[base_idx + idx] = pack_table_entry(sym as u16, cl as u8);
            }
        }
    }

    Ok((root_table, sub_tables, bitstream))
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
    huff_assign_lengths(15, HUFF_ALPHABET_SIZE, &freq, &mut code_lens);

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

    let rle_data = encode_code_lens(&code_lens);
    let mut out = Vec::with_capacity(data.len() + 2 + rle_data.len());
    out.push(max_bits as u8);
    out.push(rle_data.len() as u8);
    out.extend_from_slice(&rle_data);

    let mut bw = BitWriter::from_vec(out);
    for &b in data {
        let cl = code_widths[b as usize] as usize;
        let c = codes[b as usize];
        bw.write(c, cl);
    }

    let out = bw.finish();

    Some(out)
}

fn huff_decode(data: &[u8], expected_len: usize) -> Result<Vec<u8>, String> {
    if data.len() < 2 {
        return Err("huff: data too short for header".to_string());
    }

    let max_bits = data[0] as usize;
    if max_bits > 15 || max_bits == 0 {
        return Err(format!("huff: invalid maxBits {}", max_bits));
    }

    let rle_len = data[1] as usize;
    let header_len = 2 + rle_len;
    if data.len() < header_len {
        return Err("huff: data too short for RLE header".to_string());
    }

    let mut code_lens = [0u8; HUFF_ALPHABET_SIZE];
    decode_code_lens(&data[2..header_len], &mut code_lens)?;
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

    let mut root_table = [0u16; 2048];
    let mut sub_tables = Vec::new();
    let mut sub_table_map = [-1i16; 2048];

    for sym in 0..HUFF_ALPHABET_SIZE {
        let cl = code_lens[sym] as usize;
        if cl == 0 {
            continue;
        }
        let c = next_code[cl];
        next_code[cl] += 1;

        if cl <= 11 {
            let pad = 11 - cl;
            let start = (c << pad) as usize;
            let end = start + (1 << pad);
            for idx in start..end {
                root_table[idx] = pack_table_entry(sym as u16, cl as u8);
            }
        } else {
            let prefix = (c >> (cl - 11)) as usize;
            let sub_idx = if sub_table_map[prefix] == -1 {
                let idx = sub_tables.len() / 16;
                sub_tables.resize(sub_tables.len() + 16, 0u16);
                sub_table_map[prefix] = idx as i16;
                root_table[prefix] = pack_table_entry(idx as u16, SUB_TABLE_INDICATOR);
                idx
            } else {
                sub_table_map[prefix] as usize
            };

            let suffix = c & ((1 << (cl - 11)) - 1);
            let suffix_pad = 15 - cl;
            let start = (suffix << suffix_pad) as usize;
            let end = start + (1 << suffix_pad);
            let base_idx = sub_idx * 16;
            for idx in start..end {
                sub_tables[base_idx + idx] = pack_table_entry(sym as u16, cl as u8);
            }
        }
    }

    let sub_tables_slice = sub_tables.as_slice();
    let mut out = Vec::with_capacity(expected_len);
    let mut reader = BitReader::new(bitstream);

    while out.len() < expected_len {
        reader.fill();
        let peek15 = reader.peek(15) as usize;
        let root_idx = peek15 >> 4;
        let entry = unsafe { *root_table.get_unchecked(root_idx) };
        let (sym, bits) = unpack_table_entry(entry);
        if bits == SUB_TABLE_INDICATOR {
            let sub_idx = (sym as usize) * 16 + (peek15 & 0x0F);
            let sub_entry = unsafe { *sub_tables_slice.get_unchecked(sub_idx) };
            let (sub_sym, sub_bits) = unpack_table_entry(sub_entry);
            if sub_bits == 0 {
                return Err("huff: invalid code".to_string());
            }
            out.push(sub_sym as u8);
            reader.consume(sub_bits as usize);
        } else {
            if bits == 0 {
                return Err("huff: invalid code".to_string());
            }
            out.push(sym as u8);
            reader.consume(bits as usize);
        }
    }
    if out.len() != expected_len {
        return Err(format!("huff: got {} want {}", out.len(), expected_len));
    }
    Ok(out)
}

pub(crate) trait TableIndex: Copy + Clone + PartialEq + Send + Sync {
    const SENTINEL: Self;
    fn to_usize(self) -> usize;
    fn from_usize(val: usize) -> Self;
    fn is_sentinel(self) -> bool;
}

impl TableIndex for u16 {
    const SENTINEL: u16 = u16::MAX;
    #[inline(always)]
    fn to_usize(self) -> usize {
        self as usize
    }
    #[inline(always)]
    fn from_usize(val: usize) -> Self {
        val as u16
    }
    #[inline(always)]
    fn is_sentinel(self) -> bool {
        self == u16::MAX
    }
}

impl TableIndex for u32 {
    const SENTINEL: u32 = u32::MAX;
    #[inline(always)]
    fn to_usize(self) -> usize {
        self as usize
    }
    #[inline(always)]
    fn from_usize(val: usize) -> Self {
        val as u32
    }
    #[inline(always)]
    fn is_sentinel(self) -> bool {
        self == u32::MAX
    }
}

impl TableIndex for i32 {
    const SENTINEL: i32 = -1;
    #[inline(always)]
    fn to_usize(self) -> usize {
        self as usize
    }
    #[inline(always)]
    fn from_usize(val: usize) -> Self {
        val as i32
    }
    #[inline(always)]
    fn is_sentinel(self) -> bool {
        self == -1
    }
}

pub(crate) const LZV2_WINDOW_SIZE: usize = 65536;
const LZV2_HASH_BITS: usize = 16;
pub(crate) const LZV2_HASH_SIZE: usize = 1 << LZV2_HASH_BITS;
const LZV2_MIN_MATCH: usize = 3;
const LZV2_MAX_MATCH: usize = 258;
const LZV2_MAX_CHAIN: usize = 256;

pub fn get_hash_params(window_size: usize) -> (usize, usize) {
    let hash_bits = match window_size {
        w if w <= 65536 => 16,
        131072 => 17,
        262144 => 18,
        524288 => 19,
        _ => 20,
    };
    (hash_bits, 1 << hash_bits)
}

fn lzv2_hash(data: &[u8], pos: usize, hash_bits: usize) -> u32 {
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
        return val.wrapping_mul(0x1E35A7BD) >> (32 - hash_bits);
    }
    let val = u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
    val.wrapping_mul(0x1E35A7BD) >> (32 - hash_bits)
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
    deflate_style_encode_with_version(data, 65536, 2)
}

pub(crate) struct CompressBuffers {
    pub symbols: Vec<u16>,
    pub dist_codes: Vec<u8>,
    pub extra_bits: Vec<u8>,
}

impl CompressBuffers {
    pub fn new() -> Self {
        Self {
            symbols: Vec::new(),
            dist_codes: Vec::new(),
            extra_bits: Vec::new(),
        }
    }
}

pub fn deflate_style_encode_with_version(data: &[u8], window_size: usize, version: u8) -> Option<Vec<u8>> {
    let mut buffers = CompressBuffers::new();
    let (hash_bits, hash_size) = get_hash_params(window_size);
    if window_size <= 65536 && data.len() <= 65536 {
        let mut head = vec![u16::MAX; hash_size];
        let mut prev = vec![u16::MAX; window_size];
        deflate_style_encode_with_buffers(data, &mut head, &mut prev, &mut buffers, window_size, version, 0, hash_bits)
    } else {
        let mut head = vec![u32::MAX; hash_size];
        let mut prev = vec![u32::MAX; window_size];
        deflate_style_encode_with_buffers(data, &mut head, &mut prev, &mut buffers, window_size, version, 0, hash_bits)
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn deflate_style_encode_with_buffers<T: TableIndex>(
    data: &[u8],
    head: &mut [T],
    prev: &mut [T],
    buffers: &mut CompressBuffers,
    window_size: usize,
    version: u8,
    base: usize,
    hash_bits: usize,
) -> Option<Vec<u8>> {
    let n = data.len();
    if n < 128 {
        return None;
    }

    let max_chain_limit = match window_size {
        w if w <= 65536 => 128,
        131072 => 256,
        262144 => 512,
        524288 => 1024,
        1048576 => 1536,
        2097152 => 2048,
        4194304 => 3072,
        _ => 4096,
    };

    buffers.symbols.clear();
    buffers.dist_codes.clear();
    buffers.extra_bits.clear();

    if buffers.symbols.capacity() < n {
        buffers.symbols.reserve(n);
    }
    if buffers.dist_codes.capacity() < n / 4 {
        buffers.dist_codes.reserve(n / 4);
    }

    let symbols = &mut buffers.symbols;
    let dist_codes = &mut buffers.dist_codes;
    let extra_bits = &mut buffers.extra_bits;

    let mut bw = BitWriter::from_vec(std::mem::take(extra_bits));
    
    let mut pack_extra_bits = |extra: u16, n_extra: u8| {
        if n_extra > 0 {
            bw.write(extra as u32, n_extra as usize);
        }
    };

    let mut i = 0;
    const GOOD_MATCH: usize = 32;
    while i < n {
        if i + 64 < n {
            let h_next = lzv2_hash(data, i + 64, hash_bits) as usize;
            unsafe { std::ptr::read_volatile(&head[h_next]) };
        }
        let mut best_len = 0;
        let mut best_dist = 0;
        if i + LZV2_MIN_MATCH <= n {
            let h = lzv2_hash(data, i, hash_bits) as usize;
            let mut pos = head[h];
            prev[(base + i) & (window_size - 1)] = pos;
            head[h] = T::from_usize(base + i);
            let min_pos = (base + i).saturating_sub(window_size.min(65536)).max(base);
            let mut cl = 0;
            let mut search_depth = max_chain_limit;
            while !pos.is_sentinel() && pos.to_usize() >= min_pos && cl < search_depth {
                let p = pos.to_usize() - base;
                let mut limit = n - i;
                if limit > LZV2_MAX_MATCH {
                    limit = LZV2_MAX_MATCH;
                }
                if p >= i {
                    break;
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
                            search_depth = max_chain_limit / 8;
                        } else if l >= 8 {
                            search_depth = max_chain_limit / 2;
                        }
                    }
                }
                pos = prev[pos.to_usize() & (window_size - 1)];
                cl += 1;
            }
        }

        if best_len == 3 && best_dist > 4096 {
            best_len = 0;
        }

        if best_len >= LZV2_MIN_MATCH {
            let mut skip = false;
            if best_len < GOOD_MATCH && i + 1 + LZV2_MIN_MATCH <= n && best_len < LZV2_MAX_MATCH {
                let h2 = lzv2_hash(data, i + 1, hash_bits) as usize;
                let mut pos2 = head[h2];
                let min_pos2 = (base + i + 1).saturating_sub(window_size.min(65536)).max(base);
                let mut cl2 = 0;
                while !pos2.is_sentinel() && pos2.to_usize() >= min_pos2 && cl2 < max_chain_limit / 2 {
                    let p = pos2.to_usize() - base;
                    let mut limit = n - (i + 1);
                    if limit > LZV2_MAX_MATCH {
                        limit = LZV2_MAX_MATCH;
                    }
                    if best_len <= limit && data[p] == data[i + 1] && data[p + best_len - 1] == data[i + 1 + best_len - 1] {
                        let l = match_len_u64(data, p, i + 1, limit);
                        let d = i + 1 - p;
                        if l > best_len || (l == best_len && d < best_dist / 4) {
                            best_len = l;
                            best_dist = d;
                            skip = true;
                            break;
                        }
                    }
                    pos2 = prev[pos2.to_usize() & (window_size - 1)];
                    cl2 += 1;
                }
            }

            if skip {
                symbols.push(data[i] as u16);
                i += 1;
                let h3 = lzv2_hash(data, i, hash_bits) as usize;
                prev[(base + i) & (window_size - 1)] = head[h3];
                head[h3] = T::from_usize(base + i);

                // Lazy-2: check if we should skip i again in favor of i+1
                let mut skip2 = false;
                if best_len < GOOD_MATCH && i + 1 + LZV2_MIN_MATCH <= n && best_len < LZV2_MAX_MATCH {
                    let h2 = lzv2_hash(data, i + 1, hash_bits) as usize;
                    let mut pos2 = head[h2];
                    let min_pos2 = (base + i + 1).saturating_sub(window_size.min(65536)).max(base);
                    let mut cl2 = 0;
                    while !pos2.is_sentinel() && pos2.to_usize() >= min_pos2 && cl2 < max_chain_limit / 2 {
                        let p = pos2.to_usize() - base;
                        let mut limit = n - (i + 1);
                        if limit > LZV2_MAX_MATCH {
                            limit = LZV2_MAX_MATCH;
                        }
                        if best_len <= limit && data[p] == data[i + 1] && data[p + best_len - 1] == data[i + 1 + best_len - 1] {
                            let l = match_len_u64(data, p, i + 1, limit);
                            let d = i + 1 - p;
                            if l > best_len || (l == best_len && d < best_dist / 4) {
                                best_len = l;
                                best_dist = d;
                                skip2 = true;
                                break;
                            }
                        }
                        pos2 = prev[pos2.to_usize() & (window_size - 1)];
                        cl2 += 1;
                    }
                }

                if skip2 {
                    symbols.push(data[i] as u16);
                    i += 1;
                    let h3 = lzv2_hash(data, i, hash_bits) as usize;
                    prev[(base + i) & (window_size - 1)] = head[h3];
                    head[h3] = T::from_usize(base + i);
                }
            }

            let (len_code, len_extra, len_n_extra) = len_to_code(best_len as u16);
            let (dist_code, dist_extra, dist_n_extra) = dist_to_code((best_dist - 1) as u16);

            symbols.push(256 + len_code as u16);
            dist_codes.push(dist_code);

            pack_extra_bits(len_extra, len_n_extra);
            pack_extra_bits(dist_extra, dist_n_extra);

            let step = if best_len >= 64 { 4 } else { 1 };
            for j in (1..best_len).step_by(step) {
                if i + j + LZV2_MIN_MATCH <= n {
                    let h = lzv2_hash(data, i + j, hash_bits) as usize;
                    prev[(base + i + j) & (window_size - 1)] = head[h];
                    head[h] = T::from_usize(base + i + j);
                }
            }
            i += best_len;
        } else {
            symbols.push(data[i] as u16);
            i += 1;
        }
    }

    *extra_bits = bw.finish();

    let sym_comp = huff_encode_uint16(symbols, version)?;
    let mut dist_comp = Vec::new();
    if !dist_codes.is_empty() {
        if let Some(comp) = huff_encode(dist_codes) {
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
    out.extend_from_slice(extra_bits);

    Some(out)
}

pub(crate) fn deflate_style_decode(data: &[u8], expected_len: usize) -> Result<Vec<u8>, String> {
    deflate_style_decode_with_version(data, expected_len, 2)
}

pub(crate) fn deflate_style_decode_with_version(data: &[u8], expected_len: usize, version: u8) -> Result<Vec<u8>, String> {
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
    let (root_table, sub_tables, bitstream) = build_huff16_tables_and_stream(sym_cd, version)?;
    let sub_tables_slice = sub_tables.as_slice();
    let mut reader = BitReader::new(bitstream);

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

    for _ in 0..num_syms {
        reader.fill();
        let peek15 = reader.peek(15) as usize;
        let root_idx = peek15 >> 4;
        let entry = unsafe { *root_table.get_unchecked(root_idx) };
        let (mut sym, mut bits) = unpack_table_entry(entry);
        if bits == SUB_TABLE_INDICATOR {
            let sub_idx = (sym as usize) * 16 + (peek15 & 0x0F);
            let sub_entry = unsafe { *sub_tables_slice.get_unchecked(sub_idx) };
            let (sub_sym, sub_bits) = unpack_table_entry(sub_entry);
            if sub_bits == 0 {
                return Err("huffUint16: invalid code".to_string());
            }
            sym = sub_sym;
            bits = sub_bits;
        } else if bits == 0 {
            return Err("huffUint16: invalid code".to_string());
        }
        reader.consume(bits as usize);

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
            out.reserve(ml + 8);
            let dst_orig = out.len();
            let mut src = dst_orig - dist;
            let mut dst = dst_orig;
            let end = dst_orig + ml;
            unsafe {
                if dist == 1 {
                    let v = *out.as_ptr().add(src) as u64;
                    let v8 = v * 0x0101010101010101;
                    while dst < end {
                        std::ptr::write_unaligned(out.as_mut_ptr().add(dst) as *mut u64, v8);
                        dst += 8;
                    }
                } else if dist < 8 {
                    while dst < end {
                        *out.as_mut_ptr().add(dst) = *out.as_ptr().add(src);
                        src += 1;
                        dst += 1;
                    }
                } else {
                    while dst < end {
                        std::ptr::copy_nonoverlapping(out.as_ptr().add(src), out.as_mut_ptr().add(dst), 8);
                        src += 8;
                        dst += 8;
                    }
                }
                out.set_len(end);
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

pub(crate) const BLOCK_SIZE: usize = 128 * 1024; // 128KB blocks

// Block-size caps applied at encode time. Block boundaries are self-describing in
// the stream (each block carries its own comp/orig sizes), so capping only affects
// the encoder and never breaks decode of existing or new archives.
//
// Plain/blocked (e.g. text): large blocks amortize per-block Huffman-table overhead,
// so we allow up to 8 MB — beyond that the ratio is flat while a single giant block
// kills parallelism. Shuffle modes (e.g. float weights) are the opposite: each block
// gets one Huffman tree, and a block spanning many stride-lanes mixes incompatible
// byte distributions into that tree, hurting the ratio. Keeping shuffle blocks small
// lets each tree fit a homogeneous slice.
pub(crate) const MAX_PLAIN_BLOCK_SIZE: usize = 8 * 1024 * 1024;
pub(crate) const MAX_SHUFFLE_BLOCK_SIZE: usize = 1024 * 1024;

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
        return (0..n).map(&f).collect();
    }

    use std::mem::MaybeUninit;
    let mut results: Vec<MaybeUninit<T>> = (0..n).map(|_| MaybeUninit::uninit()).collect();
    let chunk_size = n.div_ceil(threads);
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

pub(crate) fn encode_block_result(block: &[u8], c_opt: Option<Vec<u8>>, lane: usize, _version: u8) -> Vec<u8> {
    let header_size = 10;
    match c_opt {
        Some(c) if c.len() < block.len() => {
            let mut v = Vec::with_capacity(c.len() + header_size);
            v.extend_from_slice(&(c.len() as u32).to_le_bytes());
            v.extend_from_slice(&(block.len() as u32).to_le_bytes());
            v.push(1); // compressed flag
            v.push(lane as u8);
            v.extend_from_slice(&c);
            v
        }
        _ => {
            let mut v = Vec::with_capacity(block.len() + header_size);
            v.extend_from_slice(&(block.len() as u32).to_le_bytes());
            v.extend_from_slice(&(block.len() as u32).to_le_bytes());
            v.push(0); // raw flag
            v.push(lane as u8);
            v.extend_from_slice(block);
            v
        }
    }
}

fn deflate_blocked_encode(data: &[u8]) -> Option<Vec<u8>> {
    deflate_blocked_encode_with_version(data, 65536, 128 * 1024, 2)
}

/// Content-defined block boundaries (target size `block_size`).
///
/// Blocks in the stream are self-describing (each stores its own orig/comp size),
/// so making the *encoder* cut on content instead of fixed offsets keeps the exact
/// same on-disk format and decoder, while making the output shift-resistant: an
/// insertion/deletion only reshapes the block(s) it lands in, so unchanged regions
/// stay byte-identical and dedup (Xet) across revisions and forks.
fn cdc_block_bounds(data: &[u8], block_size: usize) -> Vec<(usize, usize)> {
    let gear = cdc_gear_table();
    let avg = block_size.max(1024).next_power_of_two();
    let min = (avg / 4).max(512);
    let max = avg.saturating_mul(4);
    let bits = (avg as u64).trailing_zeros();
    let mask_s = (1u64 << (bits + 2)) - 1;
    let mask_l = (1u64 << bits.saturating_sub(2)) - 1;
    let n = data.len();
    let mut bounds = Vec::new();
    let mut i = 0;
    while i < n {
        let end = (i + max).min(n);
        if end - i <= min {
            bounds.push((i, end));
            i = end;
            continue;
        }
        let center = (i + avg).min(end);
        let mut fp: u64 = 0;
        let mut cut = end;
        let mut j = i + min;
        while j < end {
            fp = (fp << 1).wrapping_add(gear[data[j] as usize]);
            j += 1;
            if j < center {
                if fp & mask_s == 0 {
                    cut = j;
                    break;
                }
            } else if fp & mask_l == 0 {
                cut = j;
                break;
            }
        }
        bounds.push((i, cut));
        i = cut;
    }
    bounds
}

/// Block boundaries for the blocked encoder.
///
/// Content-defined boundaries are the **default**: same on-disk format, same
/// decoder, same compression ratio, but shift-resistant so revisions/forks dedup
/// under Xet-style chunking. Set `BNC_FIXED_BLOCKS=1` to fall back to the old
/// fixed-size boundaries (kept as an escape hatch for A/B testing).
fn block_boundaries(data: &[u8], block_size: usize) -> Vec<(usize, usize)> {
    let n = data.len();
    if std::env::var_os("BNC_FIXED_BLOCKS").is_some() {
        let nb = n.div_ceil(block_size);
        (0..nb).map(|b| (b * block_size, ((b + 1) * block_size).min(n))).collect()
    } else {
        cdc_block_bounds(data, block_size)
    }
}

pub fn deflate_blocked_encode_with_version(
    data: &[u8],
    window_size: usize,
    block_size: usize,
    version: u8,
) -> Option<Vec<u8>> {
    let n = data.len();
    if n < 128 {
        return None;
    }
    if window_size <= 65536 && block_size <= 65536 {
        deflate_blocked_encode_with_version_impl::<u16>(data, window_size, block_size, version)
    } else {
        deflate_blocked_encode_with_version_impl::<u32>(data, window_size, block_size, version)
    }
}

fn deflate_blocked_encode_with_version_impl<T: TableIndex>(
    data: &[u8],
    window_size: usize,
    block_size: usize,
    version: u8,
) -> Option<Vec<u8>> {
    let n = data.len();
    let bounds = block_boundaries(data, block_size);
    let num_blocks = bounds.len();
    let threads = num_threads(num_blocks);
    let (hash_bits, hash_size) = get_hash_params(window_size);

    use std::mem::MaybeUninit;
    let mut encoded_blocks: Vec<MaybeUninit<Vec<u8>>> = (0..num_blocks)
        .map(|_| MaybeUninit::uninit())
        .collect();

    if threads <= 1 {
        let mut head = vec![T::SENTINEL; hash_size];
        let mut prev = vec![T::SENTINEL; window_size];
        let mut buffers = CompressBuffers::new();
        let mut base = 0;
        for (b, &(start, end)) in bounds.iter().enumerate() {
            let block = &data[start..end];
            let c_opt = deflate_style_encode_with_buffers(block, &mut head, &mut prev, &mut buffers, window_size, version, base, hash_bits);
            let res = encode_block_result(block, c_opt, 0, version);
            encoded_blocks[b].write(res);
            base += end - start;
        }
    } else {
        let chunk_size = num_blocks.div_ceil(threads);
        let bounds_ref = &bounds;
        std::thread::scope(|s| {
            let mut start_idx = 0usize;
            for chunk in encoded_blocks.chunks_mut(chunk_size) {
                let cstart = start_idx;
                start_idx += chunk.len();
                s.spawn(move || {
                    let mut head = vec![T::SENTINEL; hash_size];
                    let mut prev = vec![T::SENTINEL; window_size];
                    let mut buffers = CompressBuffers::new();
                    // `base` is relative to this thread's fresh head/prev session; matches are
                    // clamped to the current block start, so blocks stay independently decodable.
                    let mut base = 0;
                    for (j, slot) in chunk.iter_mut().enumerate() {
                        let (block_start, block_end) = bounds_ref[cstart + j];
                        let block = &data[block_start..block_end];
                        let c_opt = deflate_style_encode_with_buffers(block, &mut head, &mut prev, &mut buffers, window_size, version, base, hash_bits);
                        let res = encode_block_result(block, c_opt, 0, version);
                        slot.write(res);
                        base += block_end - block_start;
                    }
                });
            }
        });
    }

    let encoded_blocks: Vec<Vec<u8>> = unsafe {
        let mut me = std::mem::ManuallyDrop::new(encoded_blocks);
        Vec::from_raw_parts(
            me.as_mut_ptr() as *mut Vec<u8>,
            me.len(),
            me.capacity(),
        )
    };

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
    deflate_blocked_decode_with_version(data, expected_len, 2)
}

fn deflate_blocked_decode_with_version(data: &[u8], expected_len: usize, version: u8) -> Result<Vec<u8>, String> {
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
        if pos + 10 > data.len() {
            return Err(format!("blocked: block {} header overrun", i));
        }
        let comp_size = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
        let orig_size = u32::from_le_bytes(data[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let flag = data[pos + 8];
        let _lane = data[pos + 9]; // read the lane byte to advance correctly
        pos += 10;

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
            decode_block_into_with_version(slot, comp, *orig, *flag, version)?;
        }
    } else {
        let has_error = std::sync::atomic::AtomicBool::new(false);
        let chunk_size = work.len().div_ceil(threads);
        let has_error_ref = &has_error;
        let results = std::thread::scope(|s| {
            let mut handles = Vec::new();
            for chunk in work.chunks_mut(chunk_size) {
                let handle = s.spawn(move || -> Result<(), String> {
                    for (slot, comp, orig, flag) in chunk.iter_mut() {
                        if has_error_ref.load(std::sync::atomic::Ordering::Relaxed) {
                            return Ok(());
                        }
                        if let Err(e) = decode_block_into_with_version(slot, comp, *orig, *flag, version) {
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
fn decode_block_into_with_version(slot: &mut [u8], comp: &[u8], orig: usize, flag: u8, version: u8) -> Result<(), String> {
    if flag == 0 {
        if comp.len() != orig {
            return Err("blocked: raw block size mismatch".to_string());
        }
        slot.copy_from_slice(comp);
    } else {
        let decoded = deflate_style_decode_with_version(comp, orig, version)?;
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
    pub fn from_u8(v: u8) -> Option<Self> {
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

fn estimate_entropy(data: &[u8]) -> f64 {
    let mut freq = [0u32; 256];
    for &b in data {
        freq[b as usize] += 1;
    }
    let n = data.len() as f64;
    freq.iter()
        .filter(|&&f| f > 0)
        .map(|&f| {
            let p = f as f64 / n;
            -p * p.log2()
        })
        .sum()
}

pub fn smart_compress(data: &[u8]) -> Option<(Vec<u8>, CompressMethod)> {
    smart_compress_with_version(data, 65536, 128 * 1024, 2)
}

pub fn smart_compress_with_version(
    data: &[u8],
    window_size: usize,
    block_size: usize,
    version: u8,
) -> Option<(Vec<u8>, CompressMethod)> {
    let n = data.len();
    if n < 128 {
        return None;
    }

    let skip_shuffle = estimate_entropy(data) > 7.5;
    // Cap block size per method family (see MAX_*_BLOCK_SIZE). Never enlarges the
    // caller's block_size, so low levels (small blocks) are unaffected.
    let plain_block = block_size.min(MAX_PLAIN_BLOCK_SIZE);
    let shuffle_block = block_size.min(MAX_SHUFFLE_BLOCK_SIZE);
    let mut best: Option<(Vec<u8>, CompressMethod)> = None;

    let consider = |cand: Option<Vec<u8>>, m: CompressMethod, best: &mut Option<(Vec<u8>, CompressMethod)>| {
        if let Some(c) = cand {
            let len = c.len();
            if best.is_none() || len < best.as_ref().unwrap().0.len() {
                *best = Some((c, m));
            }
        }
    };

    const PARALLEL_ONLY_THRESHOLD: usize = 1 << 20; // 1 MB

    if n >= PARALLEL_ONLY_THRESHOLD {
        const SAMPLE_SIZE: usize = 256 * 1024;
        let sample = if n > SAMPLE_SIZE { &data[..SAMPLE_SIZE] } else { data };
        
        let mut block_cand = None;
        let mut shuf_blk_cand = None;
        let mut shuf2_blk_cand = None;

        std::thread::scope(|s| {
            let h1 = s.spawn(|| deflate_blocked_encode_with_version(sample, window_size, block_size, version));
            let h2 = if !skip_shuffle {
                Some(s.spawn(|| {
                    let shuf4 = byte_shuffle(sample, 4);
                    deflate_blocked_encode_with_version(&shuf4, window_size, block_size, version)
                }))
            } else { None };
            let h3 = if !skip_shuffle {
                Some(s.spawn(|| {
                    let shuf2 = byte_shuffle(sample, 2);
                    deflate_blocked_encode_with_version(&shuf2, window_size, block_size, version)
                }))
            } else { None };

            block_cand = h1.join().unwrap();
            if let Some(h) = h2 { shuf_blk_cand = h.join().unwrap(); }
            if let Some(h) = h3 { shuf2_blk_cand = h.join().unwrap(); }
        });

        let mut sample_best = None;
        consider(block_cand, CompressMethod::Blocked, &mut sample_best);
        consider(shuf_blk_cand, CompressMethod::ShuffleBlk, &mut sample_best);
        consider(shuf2_blk_cand, CompressMethod::Shuffle2Blk, &mut sample_best);
        
        let best_method = sample_best.map(|(_, m)| m).unwrap_or(CompressMethod::Blocked);
        
        let final_cand = match best_method {
            CompressMethod::Blocked => deflate_blocked_encode_with_version(data, window_size, plain_block, version),
            CompressMethod::ShuffleBlk => {
                let shuf4 = byte_shuffle(data, 4);
                deflate_blocked_encode_with_version(&shuf4, window_size, shuffle_block, version)
            }
            CompressMethod::Shuffle2Blk => {
                let shuf2 = byte_shuffle(data, 2);
                deflate_blocked_encode_with_version(&shuf2, window_size, shuffle_block, version)
            }
            _ => None,
        };
        return final_cand.map(|c| (c, best_method));
    } else {
        let mut plain_cand = None;
        let mut shuf_cand = None;
        let mut shuf2_cand = None;
        let mut blk_cand = None;
        let mut shuf_blk_cand = None;
        let mut shuf2_blk_cand = None;

        std::thread::scope(|s| {
            let h_plain = s.spawn(|| deflate_style_encode_with_version(data, window_size, version));
            let h_shuf = if !skip_shuffle {
                Some(s.spawn(move || {
                    let shuf4 = byte_shuffle(data, 4);
                    deflate_style_encode_with_version(&shuf4, window_size, version)
                }))
            } else {
                None
            };
            let h_shuf2 = if !skip_shuffle {
                Some(s.spawn(move || {
                    let shuf2 = byte_shuffle(data, 2);
                    deflate_style_encode_with_version(&shuf2, window_size, version)
                }))
            } else {
                None
            };

            let h_blk = if n >= plain_block {
                Some(s.spawn(move || deflate_blocked_encode_with_version(data, window_size, plain_block, version)))
            } else {
                None
            };
            let h_shuf_blk = if n >= shuffle_block && !skip_shuffle {
                Some(s.spawn(move || {
                    let shuf4 = byte_shuffle(data, 4);
                    deflate_blocked_encode_with_version(&shuf4, window_size, shuffle_block, version)
                }))
            } else {
                None
            };
            let h_shuf2_blk = if n >= shuffle_block && !skip_shuffle {
                Some(s.spawn(move || {
                    let shuf2 = byte_shuffle(data, 2);
                    deflate_blocked_encode_with_version(&shuf2, window_size, shuffle_block, version)
                }))
            } else {
                None
            };

            plain_cand = h_plain.join().unwrap();
            if let Some(h) = h_shuf {
                shuf_cand = h.join().unwrap();
            }
            if let Some(h) = h_shuf2 {
                shuf2_cand = h.join().unwrap();
            }
            if let Some(h) = h_blk {
                blk_cand = h.join().unwrap();
            }
            if let Some(h) = h_shuf_blk {
                shuf_blk_cand = h.join().unwrap();
            }
            if let Some(h) = h_shuf2_blk {
                shuf2_blk_cand = h.join().unwrap();
            }
        });

        consider(plain_cand, CompressMethod::Plain, &mut best);
        consider(shuf_cand, CompressMethod::Shuffle, &mut best);
        consider(shuf2_cand, CompressMethod::Shuffle2, &mut best);
        consider(blk_cand, CompressMethod::Blocked, &mut best);
        consider(shuf_blk_cand, CompressMethod::ShuffleBlk, &mut best);
        consider(shuf2_blk_cand, CompressMethod::Shuffle2Blk, &mut best);
    }

    best
}

pub fn smart_decompress(data: &[u8], method: CompressMethod, expected_len: usize) -> Result<Vec<u8>, String> {
    smart_decompress_with_version(data, method, expected_len, 2)
}

pub fn smart_decompress_with_version(data: &[u8], method: CompressMethod, expected_len: usize, version: u8) -> Result<Vec<u8>, String> {
    match method {
        CompressMethod::Plain => deflate_style_decode_with_version(data, expected_len, version),
        CompressMethod::Blocked => deflate_blocked_decode_with_version(data, expected_len, version),
        CompressMethod::Shuffle => {
            let decoded = deflate_style_decode_with_version(data, expected_len, version)?;
            Ok(byte_unshuffle(&decoded, 4))
        }
        CompressMethod::ShuffleBlk => {
            let decoded = deflate_blocked_decode_with_version(data, expected_len, version)?;
            Ok(byte_unshuffle(&decoded, 4))
        }
        CompressMethod::Shuffle2 => {
            let decoded = deflate_style_decode_with_version(data, expected_len, version)?;
            Ok(byte_unshuffle(&decoded, 2))
        }
        CompressMethod::Shuffle2Blk => {
            let decoded = deflate_blocked_decode_with_version(data, expected_len, version)?;
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

pub enum DecompressState {
    Uninitialized,
    Raw {
        remaining: u64,
    },
    Buffered {
        data: Vec<u8>,
        pos: usize,
    },
    Blocked {
        num_blocks: usize,
        blocks_read: usize,
        buffer: Vec<Vec<u8>>,
        buf_idx: usize,
        buf_pos: usize,
    },
    ShuffleBlocked {
        stride: usize,
        groups: usize,
        prev_byte: [u8; 4],
        active_blocks: Vec<std::collections::VecDeque<Vec<u8>>>,
        num_blocks_total: usize,
        blocks_read: usize,
        current_idx: usize,
        buffer: Vec<u8>,
        buf_pos: usize,
    },
}

pub struct DecompressReader<R: Read + Seek> {
    inner: R,
    method: CompressMethod,
    stored_size: u64,
    orig_size: u64,
    stored_raw: bool,
    version: u8,
    state: DecompressState,
}

impl<R: Read + Seek> DecompressReader<R> {
    pub fn new(
        inner: R,
        method: CompressMethod,
        stored_size: u64,
        orig_size: u64,
        stored_raw: bool,
        version: u8,
    ) -> Self {
        Self {
            inner,
            method,
            stored_size,
            orig_size,
            stored_raw,
            version,
            state: DecompressState::Uninitialized,
        }
    }

    fn ensure_initialized(&mut self) -> io::Result<()> {
        if !matches!(self.state, DecompressState::Uninitialized) {
            return Ok(());
        }

        if self.stored_raw {
            self.state = DecompressState::Raw {
                remaining: self.stored_size,
            };
            return Ok(());
        }

        match self.method {
            CompressMethod::Plain | CompressMethod::Shuffle | CompressMethod::Shuffle2 => {
                let mut payload = vec![0u8; self.stored_size as usize];
                self.inner.read_exact(&mut payload)?;

                let decoded = smart_decompress_with_version(&payload, self.method, self.orig_size as usize, self.version)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

                self.state = DecompressState::Buffered {
                    data: decoded,
                    pos: 0,
                };
            }
            CompressMethod::Blocked => {
                let mut num_blocks_bytes = [0u8; 4];
                self.inner.read_exact(&mut num_blocks_bytes)?;
                let num_blocks = u32::from_le_bytes(num_blocks_bytes) as usize;

                self.state = DecompressState::Blocked {
                    num_blocks,
                    blocks_read: 0,
                    buffer: Vec::new(),
                    buf_idx: 0,
                    buf_pos: 0,
                };
            }
            CompressMethod::ShuffleBlk | CompressMethod::Shuffle2Blk => {
                let mut num_blocks_bytes = [0u8; 4];
                self.inner.read_exact(&mut num_blocks_bytes)?;
                let num_blocks = u32::from_le_bytes(num_blocks_bytes) as usize;
                
                let stride = if self.method == CompressMethod::ShuffleBlk { 4 } else { 2 };
                let groups = (self.orig_size as usize) / stride;

                self.state = DecompressState::ShuffleBlocked {
                    stride,
                    groups,
                    prev_byte: [0u8; 4],
                    active_blocks: vec![std::collections::VecDeque::new(); stride],
                    num_blocks_total: num_blocks,
                    blocks_read: 0,
                    current_idx: 0,
                    buffer: Vec::new(),
                    buf_pos: 0,
                };
            }
        }

        Ok(())
    }
}

impl<R: Read + Seek> Read for DecompressReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.ensure_initialized()?;

        match &mut self.state {
            DecompressState::Uninitialized => unreachable!(),
            DecompressState::Raw { remaining } => {
                if *remaining == 0 {
                    return Ok(0);
                }
                let to_read = std::cmp::min(*remaining, buf.len() as u64) as usize;
                let n = self.inner.read(&mut buf[..to_read])?;
                *remaining -= n as u64;
                Ok(n)
            }
            DecompressState::Buffered { data, pos } => {
                if *pos >= data.len() {
                    return Ok(0);
                }
                let to_copy = std::cmp::min(data.len() - *pos, buf.len());
                buf[..to_copy].copy_from_slice(&data[*pos..*pos + to_copy]);
                *pos += to_copy;
                Ok(to_copy)
            }
            DecompressState::Blocked {
                num_blocks,
                blocks_read,
                buffer,
                buf_idx,
                buf_pos,
            } => {
                if *buf_idx >= buffer.len() {
                    if *blocks_read >= *num_blocks {
                        return Ok(0);
                    }
                    
                    let concurrency = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
                    let batch_size = concurrency * 4;
                    let to_fetch = std::cmp::min(batch_size, *num_blocks - *blocks_read);
                    
                    let mut batch_comp = Vec::with_capacity(to_fetch);
                    let mut header_buf = [0u8; 10];
                    for _ in 0..to_fetch {
                        self.inner.read_exact(&mut header_buf)?;
                        let comp_size = u32::from_le_bytes([header_buf[0], header_buf[1], header_buf[2], header_buf[3]]) as usize;
                        let orig_size = u32::from_le_bytes([header_buf[4], header_buf[5], header_buf[6], header_buf[7]]) as usize;
                        let flag = header_buf[8];

                        let mut comp = vec![0u8; comp_size];
                        self.inner.read_exact(&mut comp)?;
                        batch_comp.push((comp, orig_size, flag));
                    }
                    *blocks_read += to_fetch;
                    
                    let version = self.version;
                    let decomp_batch = std::thread::scope(|s| {
                        let mut handles = Vec::with_capacity(to_fetch);
                        for (comp, orig_size, flag) in batch_comp {
                            handles.push(s.spawn(move || {
                                if flag == 1 {
                                    deflate_style_decode_with_version(&comp, orig_size, version)
                                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
                                } else {
                                    Ok(comp)
                                }
                            }));
                        }
                        let mut results = Vec::with_capacity(to_fetch);
                        for h in handles {
                            results.push(h.join().unwrap()?);
                        }
                        Ok::<Vec<Vec<u8>>, io::Error>(results)
                    })?;
                    
                    *buffer = decomp_batch;
                    *buf_idx = 0;
                    *buf_pos = 0;
                }
                
                let current_buf = &buffer[*buf_idx];
                let to_copy = std::cmp::min(current_buf.len() - *buf_pos, buf.len());
                buf[..to_copy].copy_from_slice(&current_buf[*buf_pos..*buf_pos + to_copy]);
                *buf_pos += to_copy;
                
                if *buf_pos >= current_buf.len() {
                    *buf_idx += 1;
                    *buf_pos = 0;
                }
                
                Ok(to_copy)
            }
            DecompressState::ShuffleBlocked {
                stride,
                groups,
                prev_byte,
                active_blocks,
                num_blocks_total,
                blocks_read,
                current_idx,
                buffer,
                buf_pos,
            } => {
                let orig_size_usize = self.orig_size as usize;
                
                if *buf_pos >= buffer.len() {
                    if *current_idx >= orig_size_usize {
                        return Ok(0);
                    }
                    
                    while active_blocks.iter().any(|q| q.is_empty()) && *blocks_read < *num_blocks_total {
                        let concurrency = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
                        let batch_size = concurrency * 4;
                        
                        let mut batch_comp = Vec::new();
                        let to_fetch = std::cmp::min(batch_size, *num_blocks_total - *blocks_read);
                        
                        let mut header_buf = [0u8; 10];
                        for _ in 0..to_fetch {
                            self.inner.read_exact(&mut header_buf)?;
                            let comp_size = u32::from_le_bytes([header_buf[0], header_buf[1], header_buf[2], header_buf[3]]) as usize;
                            let orig_size = u32::from_le_bytes([header_buf[4], header_buf[5], header_buf[6], header_buf[7]]) as usize;
                            let flag = header_buf[8];
                            let lane_id = std::cmp::min(header_buf[9] as usize, *stride - 1);
                            
                            let mut comp_buf = vec![0u8; comp_size];
                            self.inner.read_exact(&mut comp_buf)?;
                            batch_comp.push((lane_id, comp_buf, orig_size, flag));
                        }
                        *blocks_read += to_fetch;
                        
                        if !batch_comp.is_empty() {
                            let version = self.version;
                            let decomp_batch = std::thread::scope(|scope| {
                                let num_threads = std::cmp::min(concurrency, batch_comp.len());
                                let chunk_size = batch_comp.len().div_ceil(num_threads);
                                let mut handles = Vec::with_capacity(num_threads);
                                
                                for chunk in batch_comp.chunks(chunk_size) {
                                    let c = chunk.to_vec();
                                    handles.push(scope.spawn(move || {
                                        let mut res = Vec::with_capacity(c.len());
                                        for (s, comp, orig_size, flag) in c {
                                            let mut decoded = if flag == 1 {
                                                deflate_style_decode_with_version(&comp, orig_size, version)
                                                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
                                            } else {
                                                comp
                                            };
                                            
                                            if version >= 3 {
                                                let mut p = 0u8;
                                                for i in 0..decoded.len() {
                                                    let val = decoded[i].wrapping_add(p);
                                                    decoded[i] = val;
                                                    p = val;
                                                }
                                            }
                                            res.push((s, decoded));
                                        }
                                        Ok::<_, io::Error>(res)
                                    }));
                                }
                                
                                let mut results = Vec::with_capacity(batch_comp.len());
                                for h in handles {
                                    results.extend(h.join().unwrap()?);
                                }
                                Ok::<_, io::Error>(results)
                            })?;
                            
                            for (s, decoded) in decomp_batch {
                                active_blocks[s].push_back(decoded);
                            }
                        }
                    }

                    if *current_idx < *stride * *groups {
                        let g_start = *current_idx / *stride;
                        let curr_block_size = active_blocks[0].front().unwrap().len();
                        
                        let groups_left = *groups - g_start;
                        let groups_to_process = std::cmp::min(curr_block_size, groups_left);
                        
                        let new_len = groups_to_process * *stride;
                        buffer.clear();
                        buffer.resize(new_len, 0);
                        
                        let mut block_slices = Vec::with_capacity(*stride);
                        for s in 0..*stride {
                            block_slices.push(active_blocks[s].front().unwrap().as_slice());
                        }
                        
                        for g_offset in 0..groups_to_process {
                            if self.version >= 3 {
                                if *stride == 4 {
                                    let v0 = unsafe { *block_slices.get_unchecked(0).get_unchecked(g_offset) } as u32;
                                    let v1 = unsafe { *block_slices.get_unchecked(1).get_unchecked(g_offset) } as u32;
                                    let v2 = unsafe { *block_slices.get_unchecked(2).get_unchecked(g_offset) } as u32;
                                    let v3 = unsafe { *block_slices.get_unchecked(3).get_unchecked(g_offset) } as u32;
                                    // Depending on endianness, we might want from_le_bytes, but we just need byte 0 to be v0, byte 1 to be v1, etc.
                                    let combined = u32::from_le_bytes([v0 as u8, v1 as u8, v2 as u8, v3 as u8]);
                                    unsafe {
                                        let ptr = buffer.as_mut_ptr().add(g_offset * 4) as *mut u32;
                                        std::ptr::write_unaligned(ptr, combined);
                                    }
                                } else {
                                    for s in 0..*stride {
                                        let val = unsafe { *block_slices.get_unchecked(s).get_unchecked(g_offset) };
                                        unsafe { *buffer.get_unchecked_mut(g_offset * *stride + s) = val; }
                                    }
                                }
                            } else {
                                for s in 0..*stride {
                                    let delta = unsafe { *block_slices.get_unchecked(s).get_unchecked(g_offset) };
                                    let val = delta.wrapping_add(prev_byte[s]);
                                    prev_byte[s] = val;
                                    unsafe { *buffer.get_unchecked_mut(g_offset * *stride + s) = val; }
                                }
                            }
                        }
                        
                        *current_idx += groups_to_process * *stride;
                        *buf_pos = 0;
                        
                        let g_end = *current_idx / *stride;
                        if groups_to_process == curr_block_size || g_end == *groups {
                            for s in 0..*stride {
                                active_blocks[s].pop_front();
                            }
                        }
                    } else {
                        // Remainder bytes
                        let remaining_bytes = orig_size_usize - *current_idx;
                        let curr_block_size = active_blocks[*stride - 1].front().unwrap().len();
                        let to_process = std::cmp::min(remaining_bytes, curr_block_size);
                        
                        buffer.clear();
                        buffer.resize(to_process, 0);
                        
                        let block_slice = active_blocks[*stride - 1].front().unwrap().as_slice();
                        for i in 0..to_process {
                            let b = unsafe { *block_slice.get_unchecked(i) };
                            unsafe { *buffer.get_unchecked_mut(i) = b; }
                        }
                        
                        *current_idx += to_process;
                        *buf_pos = 0;
                        
                        if to_process == curr_block_size || *current_idx == orig_size_usize {
                            active_blocks[*stride - 1].pop_front();
                        }
                    }
                }

                let to_copy = std::cmp::min(buffer.len() - *buf_pos, buf.len());
                buf[..to_copy].copy_from_slice(&buffer[*buf_pos..*buf_pos + to_copy]);
                *buf_pos += to_copy;
                
                Ok(to_copy)
            }
        }
    }
}

// ============================================================================
// Dedup-friendly mode: content-defined chunking + per-chunk Big Bounce codec.
//
// Motivation: compressing a whole file as one stream gives a great ratio but
// destroys chunk-level deduplication (Xet/CDC) — any edit cascades through the
// stream, so a one-tensor change re-stores the whole file. Splitting the input
// into *content-defined* chunks and compressing each independently (still with
// the byte-shuffle + Huffman codec) keeps most of the ratio while making the
// stored bytes shift-resistant, so unchanged regions dedup across revisions and
// forks. Output is deterministic.
//
// Container ("BNCD"): magic[4] | version u8 | chunk_count u32le |
//   chunk_count × { orig_len u32le, stored_len u32le, method u8, raw u8 } |
//   concatenated chunk payloads.
// ============================================================================

pub(crate) const CDC_MIN: usize = 4 * 1024;
pub(crate) const CDC_AVG: usize = 16 * 1024;
pub(crate) const CDC_MAX: usize = 64 * 1024;

fn cdc_gear_table() -> [u64; 256] {
    // Deterministic table via splitmix64 (no external dependency).
    let mut t = [0u64; 256];
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    for slot in t.iter_mut() {
        x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = x;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        *slot = z ^ (z >> 31);
    }
    t
}

/// Resolve CDC parameters, allowing an env override (`BNC_CDC_AVG_KB`) for sweeps.
/// `min = avg/4`, `max = avg*4`. avg is rounded to a power of two.
fn cdc_params() -> (usize, usize, usize) {
    let avg = match std::env::var("BNC_CDC_AVG_KB").ok().and_then(|v| v.parse::<usize>().ok()) {
        Some(kb) if kb > 0 => (kb * 1024).next_power_of_two(),
        _ => CDC_AVG,
    };
    (avg / 4, avg, avg * 4)
}

/// Split `data` into content-defined chunks (FastCDC, normalized chunking).
fn cdc_chunks(data: &[u8]) -> Vec<(usize, usize)> {
    let gear = cdc_gear_table();
    let (cdc_min, cdc_avg, cdc_max) = cdc_params();
    let bits = (cdc_avg as u64).trailing_zeros();
    let mask_s = (1u64 << (bits + 2)) - 1; // stricter before the average point
    let mask_l = (1u64 << (bits - 2)) - 1; // looser after it
    let n = data.len();
    let mut chunks = Vec::new();
    let mut i = 0;
    while i < n {
        let end = (i + cdc_max).min(n);
        if end - i <= cdc_min {
            chunks.push((i, end - i));
            i = end;
            continue;
        }
        let center = (i + cdc_avg).min(end);
        let mut fp: u64 = 0;
        let mut cut = end;
        let mut j = i + CDC_MIN;
        while j < end {
            fp = (fp << 1).wrapping_add(gear[data[j] as usize]);
            j += 1;
            if j < center {
                if fp & mask_s == 0 {
                    cut = j;
                    break;
                }
            } else if fp & mask_l == 0 {
                cut = j;
                break;
            }
        }
        chunks.push((i, cut - i));
        i = cut;
    }
    chunks
}

/// Compress `data` in dedup-friendly mode (content-defined chunks, each encoded
/// independently with the Big Bounce codec). Deterministic.
pub fn cdc_compress(data: &[u8], window_size: usize, block_size: usize, version: u8) -> Vec<u8> {
    let chunks = cdc_chunks(data);
    let mut records: Vec<(u32, u32, u8, u8)> = Vec::with_capacity(chunks.len());
    let mut payload: Vec<u8> = Vec::new();
    for (off, len) in chunks {
        let raw = &data[off..off + len];
        let (stored, method, is_raw) = match smart_compress_with_version(raw, window_size, block_size, version) {
            Some((c, m)) if c.len() < raw.len() => (c, m.to_u8(), 0u8),
            _ => (raw.to_vec(), 0u8, 1u8),
        };
        records.push((len as u32, stored.len() as u32, method, is_raw));
        payload.extend_from_slice(&stored);
    }
    let mut out = Vec::with_capacity(9 + records.len() * 10 + payload.len());
    out.extend_from_slice(b"BNCD");
    out.push(version);
    out.extend_from_slice(&(records.len() as u32).to_le_bytes());
    for (orig, stored, method, is_raw) in &records {
        out.extend_from_slice(&orig.to_le_bytes());
        out.extend_from_slice(&stored.to_le_bytes());
        out.push(*method);
        out.push(*is_raw);
    }
    out.extend_from_slice(&payload);
    out
}

/// Inverse of [`cdc_compress`].
pub fn cdc_decompress(blob: &[u8]) -> Result<Vec<u8>, String> {
    if blob.len() < 9 || &blob[..4] != b"BNCD" {
        return Err("not a BNCD (dedup-friendly) stream".to_string());
    }
    let version = blob[4];
    let count = u32::from_le_bytes(blob[5..9].try_into().unwrap()) as usize;
    let mut pos = 9;
    let mut recs: Vec<(usize, usize, u8, u8)> = Vec::with_capacity(count);
    for _ in 0..count {
        if pos + 10 > blob.len() {
            return Err("truncated BNCD index".to_string());
        }
        let orig = u32::from_le_bytes(blob[pos..pos + 4].try_into().unwrap()) as usize;
        let stored = u32::from_le_bytes(blob[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let method = blob[pos + 8];
        let is_raw = blob[pos + 9];
        pos += 10;
        recs.push((orig, stored, method, is_raw));
    }
    let mut out = Vec::new();
    for (orig, stored, method, is_raw) in recs {
        if pos + stored > blob.len() {
            return Err("truncated BNCD payload".to_string());
        }
        let chunk = &blob[pos..pos + stored];
        pos += stored;
        if is_raw == 1 {
            out.extend_from_slice(chunk);
        } else {
            let m = CompressMethod::from_u8(method).ok_or("invalid method in BNCD stream")?;
            out.extend_from_slice(&smart_decompress_with_version(chunk, m, orig, version)?);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Split a blocked stream into its per-block compressed payloads.
    fn blocked_payloads(blob: &[u8]) -> Vec<Vec<u8>> {
        let num = u32::from_le_bytes(blob[0..4].try_into().unwrap()) as usize;
        let mut pos = 4;
        let mut out = Vec::with_capacity(num);
        for _ in 0..num {
            let comp = u32::from_le_bytes(blob[pos..pos + 4].try_into().unwrap()) as usize;
            pos += 10; // comp u32 + orig u32 + flag u8 + lane u8
            out.push(blob[pos..pos + comp].to_vec());
            pos += comp;
        }
        out
    }

    fn structured_floats(n: usize) -> Vec<u8> {
        let mut data = Vec::with_capacity(n * 4);
        for i in 0..n as u32 {
            let v = 0.02f32 + (i as f32 / 4000.0).sin() * 0.003 + (i as f32 / 130.0).cos() * 0.0005;
            data.extend_from_slice(&v.to_le_bytes());
        }
        data
    }

    #[test]
    fn test_cdc_blocks_shift_dedup() {
        // Content-defined blocks are the default; a byte-shift near the front must
        // leave most later blocks byte-identical (so Xet would dedup them).
        let data = structured_floats(160 * 1024); // ~640 KB
        let a = deflate_blocked_encode_with_version(&data, 65536, 128 * 1024, 2).unwrap();

        let mut shifted = data.clone();
        shifted.splice(2048..2048, (0..100u32).map(|i| (i % 256) as u8)); // insert 100 bytes
        let b = deflate_blocked_encode_with_version(&shifted, 65536, 128 * 1024, 2).unwrap();

        let pa = blocked_payloads(&a);
        let sb: std::collections::HashSet<Vec<u8>> = blocked_payloads(&b).into_iter().collect();
        let shared: usize = pa.iter().filter(|p| sb.contains(*p)).map(|p| p.len()).sum();
        let total: usize = pa.iter().map(|p| p.len()).sum();
        let pct = shared * 100 / total;
        assert!(pct >= 60, "shift deduped only {pct}% of blocked payload bytes");
    }

    #[test]
    fn test_cdc_bounds_resync_after_shift() {
        // Mechanism check (no env, deterministic): content-defined boundaries re-sync
        // after an insertion so most chunks reappear byte-identical, whereas fixed
        // boundaries shift and dedup almost nothing.
        let data = structured_floats(160 * 1024);
        let mut shifted = data.clone();
        shifted.splice(2048..2048, (0..100u32).map(|i| (i % 256) as u8));

        let cdc_a = cdc_block_bounds(&data, 128 * 1024);
        let cdc_set: std::collections::HashSet<&[u8]> =
            cdc_block_bounds(&shifted, 128 * 1024).iter().map(|&(s, e)| &shifted[s..e]).collect();
        let cdc_shared: usize =
            cdc_a.iter().map(|&(s, e)| &data[s..e]).filter(|c| cdc_set.contains(*c)).map(|c| c.len()).sum();
        let cdc_total: usize = cdc_a.iter().map(|&(s, e)| e - s).sum();
        assert!(cdc_shared * 100 / cdc_total >= 60, "cdc re-synced only {}%", cdc_shared * 100 / cdc_total);

        let fixed_set: std::collections::HashSet<&[u8]> = shifted.chunks(128 * 1024).collect();
        let fixed_shared: usize =
            data.chunks(128 * 1024).filter(|c| fixed_set.contains(*c)).map(|c| c.len()).sum();
        assert!(fixed_shared * 100 / data.len() <= 20, "fixed boundaries unexpectedly re-synced");
    }

    #[test]
    fn test_cdc_roundtrip() {
        // Build ~512 KB of float32-like structured data.
        let mut data = Vec::with_capacity(512 * 1024);
        for i in 0..(128 * 1024u32) {
            let v = 0.01f32 + (i as f32 / 50000.0).sin() * 0.001;
            data.extend_from_slice(&v.to_le_bytes());
        }
        let blob = cdc_compress(&data, 65536, 128 * 1024, 2);
        assert_eq!(&blob[..4], b"BNCD");
        assert!(blob.len() < data.len(), "cdc did not compress structured data");
        let back = cdc_decompress(&blob).unwrap();
        assert_eq!(back, data);
    }

    // Split a BNCD stream into its per-chunk payload byte-vectors.
    fn cdc_payloads(blob: &[u8]) -> Vec<Vec<u8>> {
        let count = u32::from_le_bytes(blob[5..9].try_into().unwrap()) as usize;
        let mut pos = 9;
        let mut sizes = Vec::with_capacity(count);
        for _ in 0..count {
            sizes.push(u32::from_le_bytes(blob[pos + 4..pos + 8].try_into().unwrap()) as usize);
            pos += 10;
        }
        sizes
            .into_iter()
            .map(|s| {
                let p = blob[pos..pos + s].to_vec();
                pos += s;
                p
            })
            .collect()
    }

    #[test]
    fn test_cdc_locality() {
        // Incompressible-ish data so chunk payloads are substantial.
        let mut data = vec![0u8; 256 * 1024];
        let mut x: u64 = 0x1234_5678;
        for b in data.iter_mut() {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            *b = (x >> 33) as u8;
        }
        let a = cdc_compress(&data, 65536, 128 * 1024, 2);
        data[200 * 1024] ^= 0xFF; // flip one byte near the end (in place, no shift)
        let b = cdc_compress(&data, 65536, 128 * 1024, 2);

        // Most chunk payloads must stay byte-identical -> Xet would dedup them.
        let pa = cdc_payloads(&a);
        let shared: std::collections::HashSet<Vec<u8>> = cdc_payloads(&b).into_iter().collect();
        let shared_bytes: usize = pa.iter().filter(|p| shared.contains(*p)).map(|p| p.len()).sum();
        let total: usize = pa.iter().map(|p| p.len()).sum();
        assert!(
            shared_bytes * 100 / total >= 80,
            "only {}% of payload bytes deduped after a 1-byte edit",
            shared_bytes * 100 / total
        );
    }

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

    #[test]
    fn test_entropy_screening_and_early_exit() {
        let mut rng_data = vec![0u8; 2000];
        for i in 0..2000 {
            rng_data[i] = ((i * 127 + 33) % 256) as u8;
        }
        let entropy = estimate_entropy(&rng_data);
        assert!(entropy > 7.5, "Expected high entropy, got {}", entropy);

        let compressible_data = vec![0u8; 10_000];
        let entropy_comp = estimate_entropy(&compressible_data);
        assert!(entropy_comp < 1.0, "Expected low entropy, got {}", entropy_comp);

        roundtrip(&rng_data);
        roundtrip(&compressible_data);
    }

    #[test]
    fn test_huff_assign_lengths_edge_cases() {
        // Case 1: 0 unique symbols
        let mut code_lens = [0u8; 286];
        let freq = [0usize; 286];
        huff_assign_lengths(15, 286, &freq, &mut code_lens);
        assert!(code_lens.iter().all(|&l| l == 0));

        // Case 2: 1 unique symbol
        let mut code_lens = [0u8; 286];
        let mut freq = [0usize; 286];
        freq[42] = 100;
        huff_assign_lengths(15, 286, &freq, &mut code_lens);
        assert_eq!(code_lens[42], 1);
        assert!(code_lens.iter().enumerate().all(|(idx, &l)| idx == 42 || l == 0));

        // Case 3: Fibonacci frequencies (causing a very deep tree, up to 45 symbols to avoid usize overflow)
        let mut code_lens = [0u8; 286];
        let mut freq = [0usize; 286];
        freq[0] = 1;
        freq[1] = 1;
        for i in 2..45 {
            freq[i] = freq[i - 1] + freq[i - 2];
        }
        huff_assign_lengths(15, 286, &freq, &mut code_lens);
        for (i, &l) in code_lens.iter().enumerate() {
            if freq[i] > 0 {
                assert!(l <= 15, "length {} for symbol {} exceeds 15", l, i);
                assert!(l > 0, "symbol {} with frequency {} has length 0", i, freq[i]);
            } else {
                assert_eq!(l, 0);
            }
        }
    }

    #[test]
    fn test_cross_version_roundtrip() {
        let mut data = vec![0u8; 1000];
        for i in 0..data.len() {
            data[i] = (i % 256) as u8;
        }

        // Roundtrip version 1
        if let Some((comp1, method1)) = smart_compress_with_version(&data, 65536, 131072, 1) {
            let decomp1 = smart_decompress_with_version(&comp1, method1, data.len(), 1).unwrap();
            assert_eq!(decomp1, data);
        }

        // Roundtrip version 2
        if let Some((comp2, method2)) = smart_compress_with_version(&data, 65536, 131072, 2) {
            let decomp2 = smart_decompress_with_version(&comp2, method2, data.len(), 2).unwrap();
            assert_eq!(decomp2, data);
        }
    }
}

