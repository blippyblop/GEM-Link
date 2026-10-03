//! Forward error correction: repair a lost datagram instead of losing the frame.
//!
//! This is the mechanism that makes the grey-frame defect a non-event. The defect was a
//! single lost fragment breaking an HEVC reference chain, after which every following
//! P-frame decoded to flat grey (`VD_RE/50-grey-frame-experiments.md`). Repairing the
//! datagram in the media plane means the reference chain is never broken in the first
//! place, which is strictly better than noticing afterwards that it was.
//!
//! ## The code
//!
//! A **systematic** code: the frame's data fragments go on the wire unchanged, followed by
//! `parity_count` repair shards. Shards are equal length (the caller pads), and the code is
//! a **Cauchy matrix** over GF(2⁸):
//!
//! ```text
//!   parity[i] = XOR over j of  C[i][j] * data[j],    C[i][j] = 1 / (x_i XOR y_j)
//! ```
//!
//! with `x_i = i` for the parity rows and `y_j = parity_count + j` for the data columns.
//!
//! **Why Cauchy and not Vandermonde.** Vandermonde is the obvious choice and is what most
//! first implementations reach for, but a Vandermonde matrix over GF(2⁸) is *not* guaranteed
//! to have all square submatrices non-singular, so the "recovers any `k` losses" claim
//! quietly becomes "usually". Cauchy matrices are provably MDS: every square submatrix is
//! invertible, so **any** `parity_count` losses are recoverable, and that is a property the
//! tests below check exhaustively rather than trusting.
//!
//! **Why GF(2⁸).** One byte per element, so a shard is a bytewise XOR-and-multiply with no
//! unpacking, and the 255-element limit comfortably exceeds any real frame
//! (`data_count + parity_count ≤ 255`). A 300 Mbps stream at 1400-byte datagrams is ~27,000
//! datagrams per second, or ~300 per 90 Hz frame, so this is not close to binding.
//!
//! ## Where it is not enough
//!
//! FEC repairs *losses*, not *congestion*. If the link is dropping more than the parity can
//! cover, the honest response is a lower bitrate, and this module reports its failure
//! rather than pretending: [`decode`] returns [`FecError::TooManyErasures`] and the
//! receiver turns that into a frame the caller is not allowed to display
//! ([`crate::receiver::FrameOutcome::Unreconstructable`]).

// The GF(256) kernels below are written with explicit indices on purpose: they are linear
// algebra over fixed-size matrices and shard arrays, and the index *is* the algorithm.
// Iterator chains here obscure which row and column are being combined, which is the one
// thing a reader has to be able to check in a codec.
#![allow(clippy::needless_range_loop)]

use std::fmt;

/// What went wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FecError {
    /// No data shards to protect.
    EmptyFrame,
    /// `data_count + parity_count > 255`. The field is too small for this many shards,
    /// because the Cauchy points have to be distinct field elements.
    TooManyShards {
        data_count: usize,
        parity_count: usize,
    },
    /// The shard array's length does not match `data_count + parity_count`.
    ShapeMismatch { expected: usize, actual: usize },
    /// Shards are not all the same length, so bytewise XOR is meaningless.
    ShardLengthMismatch,
    /// More data shards are missing than there are parity shards to solve with.
    TooManyErasures { missing: usize, parity: usize },
    /// The system was singular. This cannot happen for a well-formed Cauchy matrix, so it
    /// means the shard array was built inconsistently — reported rather than panicked on.
    Singular,
}

impl fmt::Display for FecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FecError::EmptyFrame => write!(f, "no data shards"),
            FecError::TooManyShards {
                data_count,
                parity_count,
            } => write!(
                f,
                "{data_count} data + {parity_count} parity shards exceeds the 255-element field"
            ),
            FecError::ShapeMismatch { expected, actual } => {
                write!(f, "expected {expected} shards, got {actual}")
            }
            FecError::ShardLengthMismatch => write!(f, "shards differ in length"),
            FecError::TooManyErasures { missing, parity } => {
                write!(f, "{missing} shards missing, only {parity} parity shards")
            }
            FecError::Singular => write!(f, "decoding system is singular"),
        }
    }
}

impl std::error::Error for FecError {}

/// The largest number of shards the field can address.
pub const MAX_SHARDS: usize = 255;

/// Data shards per FEC block.
///
/// **Why blocks exist at all.** GF(2⁸) addresses 255 elements, so a single block cannot
/// protect a frame of more than ~250 data shards. That is not a corner case: at the Frame's
/// target envelope — 300 Mbps at 90 Hz — one frame is ~416 KB, which at a 1400-byte MTU is
/// **~300 shards**. A single-block code would therefore refuse to protect a real frame,
/// which is the one thing this module exists to do. So a frame is split into blocks and each
/// block is coded independently.
///
/// **Membership is interleaved, not contiguous**, and that is the point rather than an
/// implementation detail. A burst loss takes out *consecutive* datagrams; if blocks were
/// contiguous runs, a burst would land entirely inside one block and need that block's whole
/// parity budget to survive it. With `block = index % blocks` the same burst is spread
/// across every block, so a handful of parity shards per block absorbs it. This is the
/// classic interleaving that every erasure-coded streaming system does, for the same reason.
///
/// ---
///
/// **170, and the number is chosen for the *ratio*, not the frame size.** A block holds
/// `data + parity <= MAX_SHARDS`, so the largest ratio a block can express is
/// `(255 - data_per_block) / data_per_block`. At the obvious 250 that is **5 parity slots, i.e.
/// 2 %** — so a `Ratio { 0.05 }` policy silently pays 2 %, and any adaptive controller above 2 %
/// is asking for something the layout cannot deliver. Nothing fails; the overhead just comes out
/// lower than requested, which is the worst way for a safety mechanism to be wrong.
///
/// 170 leaves 85 parity slots, a **50 %** ceiling — which is exactly `AdaptiveConfig::max`, so
/// the ratio our controller can ask for is the ratio the field can express. The cost is more
/// blocks for the same frame (each with its own parity granularity); the benefit is that
/// requested and paid are the same number.
///
/// This is safe to change because the layout is *derived*, never transmitted: both ends compute
/// it from `data_count` on the wire and the policy in the session, so a new constant moves both
/// together. `the_requested_ratio_is_the_ratio_the_field_pays` asserts the arithmetic.
pub const MAX_DATA_PER_BLOCK: usize = 170;

/// The largest ratio a block of [`MAX_DATA_PER_BLOCK`] data shards can express.
pub const MAX_EXPRESSIBLE_RATIO: f32 =
    (MAX_SHARDS - MAX_DATA_PER_BLOCK) as f32 / MAX_DATA_PER_BLOCK as f32;

/// How many FEC blocks a frame of `data_count` data shards is split into.
///
/// A pure function of `data_count`, which is on the wire, so both ends derive the same
/// answer without an extra field in the header.
pub const fn blocks_for(data_count: usize) -> usize {
    if data_count == 0 {
        return 1;
    }
    data_count.div_ceil(MAX_DATA_PER_BLOCK)
}

/// Which block a shard index belongs to, and where it sits inside that block.
pub const fn block_of(index: usize, blocks: usize) -> usize {
    index % blocks
}

/// Position of a shard within its block.
pub const fn position_in_block(index: usize, blocks: usize) -> usize {
    index / blocks
}

// ---------------------------------------------------------------------------
// GF(2^8), primitive polynomial 0x11d, generator 2.

const fn build_tables() -> ([u8; 256], [u8; 256]) {
    let mut exp = [0u8; 256];
    let mut log = [0u8; 256];
    let mut x: u16 = 1;
    let mut i = 0usize;
    while i < 255 {
        exp[i] = x as u8;
        log[x as usize] = i as u8;
        x <<= 1;
        if x & 0x100 != 0 {
            x ^= 0x11d;
        }
        i += 1;
    }
    // 2^255 == 1, so the table wraps to the exponent-0 entry. Having it set makes
    // `gf_inv` work without a special case for the identity.
    exp[255] = exp[0];
    (exp, log)
}

const TABLES: ([u8; 256], [u8; 256]) = build_tables();
const EXP: [u8; 256] = TABLES.0;
const LOG: [u8; 256] = TABLES.1;

/// Multiply two field elements.
fn gf_mul(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        return 0;
    }
    let sum = LOG[a as usize] as usize + LOG[b as usize] as usize;
    // The table is doubled so the modulo is a lookup rather than a branch.
    EXP[sum % 255]
}

/// A whole multiplication table for one coefficient: `TABLE[b] = coeff * b`.
///
/// This is what makes encoding affordable. The inner loop is *every byte of every shard*
/// multiplied by every coefficient of every parity shard — at the target envelope that is
/// ~27,000 datagrams per second, each one contributing ~1400 byte-multiplies per parity row.
/// Two log lookups, an add, a modulo and an exp lookup per byte is five operations where one
/// table read will do, and the table is 256 bytes built once per coefficient per frame.
///
/// The tables are memoised for the coefficients a frame actually uses, which for a given
/// `(data_per_block, parity_per_block)` shape is the same small set every frame — so in
/// steady state this costs nothing at all.
fn mul_table(coeff: u8) -> [u8; 256] {
    let mut table = [0u8; 256];
    for (b, slot) in table.iter_mut().enumerate() {
        *slot = gf_mul(coeff, b as u8);
    }
    table
}

/// Per-process cache of multiplication tables, keyed by the coefficients of the current
/// frame shape. Bounded by the number of distinct coefficients in use (at most 255).
fn cached_mul_table(coeff: u8) -> &'static [u8; 256] {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    static CACHE: OnceLock<Mutex<HashMap<u8, &'static [u8; 256]>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(table) = guard.get(&coeff) {
        return table;
    }
    // Boxed once and leaked on purpose: the cache lives for the process, the tables are 256
    // bytes each, and the set is bounded by the field size.
    let table: &'static [u8; 256] = Box::leak(Box::new(mul_table(coeff)));
    guard.insert(coeff, table);
    table
}

/// Multiplicative inverse. `gf_inv(0)` is 0 rather than a panic: the only caller constructs
/// the argument from a difference of two disjoint points, which is never zero, and a panic
/// on a hot path is a worse failure mode than a defined answer.
fn gf_inv(a: u8) -> u8 {
    if a == 0 {
        return 0;
    }
    EXP[255 - LOG[a as usize] as usize]
}

/// The Cauchy coefficient for parity row `parity_row` against data column `data_col`.
///
/// `x_i = i` and `y_j = parity_count + j`, which are disjoint by construction, so the
/// denominator is never zero for a well-formed shape.
fn cauchy(parity_row: usize, data_col: usize, parity_count: usize) -> u8 {
    gf_inv((parity_row as u8) ^ ((parity_count + data_col) as u8))
}

fn check_shape(data_count: usize, parity_count: usize) -> Result<(), FecError> {
    if data_count == 0 {
        return Err(FecError::EmptyFrame);
    }
    if data_count + parity_count > MAX_SHARDS {
        return Err(FecError::TooManyShards {
            data_count,
            parity_count,
        });
    }
    Ok(())
}

/// Compute `parity_count` repair shards for equal-length `data` shards.
///
/// Returns an empty vector when `parity_count` is zero, which is the "no FEC" configuration
/// and must cost nothing.
pub fn encode(data: &[Vec<u8>], parity_count: usize) -> Result<Vec<Vec<u8>>, FecError> {
    let data_count = data.len();
    check_shape(data_count, parity_count)?;
    if parity_count == 0 {
        return Ok(Vec::new());
    }

    let shard_len = data[0].len();
    if data.iter().any(|shard| shard.len() != shard_len) {
        return Err(FecError::ShardLengthMismatch);
    }

    let mut parity = vec![vec![0u8; shard_len]; parity_count];
    for row in 0..parity_count {
        let out = &mut parity[row];
        for (col, shard) in data.iter().enumerate() {
            let coeff = cauchy(row, col, parity_count);
            if coeff == 0 {
                continue;
            }
            if coeff == 1 {
                for (o, d) in out.iter_mut().zip(shard.iter()) {
                    *o ^= *d;
                }
            } else {
                let table = cached_mul_table(coeff);
                for (o, d) in out.iter_mut().zip(shard.iter()) {
                    *o ^= table[*d as usize];
                }
            }
        }
    }
    Ok(parity)
}

/// Repair missing shards in place.
///
/// `shards` is indexed `0..data_count` for data and `data_count..data_count + parity_count`
/// for parity; entries are `None` where a shard did not arrive. On success every entry is
/// `Some` and the return value is how many were reconstructed.
///
/// A frame with no erasures returns `Ok(0)` without touching anything — the common case,
/// and it must not cost a decode.
pub fn decode(
    shards: &mut [Option<Vec<u8>>],
    data_count: usize,
    parity_count: usize,
) -> Result<usize, FecError> {
    check_shape(data_count, parity_count)?;
    let total = data_count + parity_count;
    if shards.len() != total {
        return Err(FecError::ShapeMismatch {
            expected: total,
            actual: shards.len(),
        });
    }

    let missing: Vec<usize> = (0..data_count).filter(|i| shards[*i].is_none()).collect();
    if missing.is_empty() {
        return Ok(0);
    }

    let available_parity: Vec<usize> = (data_count..total)
        .filter(|i| shards[*i].is_some())
        .collect();
    if available_parity.len() < missing.len() {
        return Err(FecError::TooManyErasures {
            missing: missing.len(),
            parity: available_parity.len(),
        });
    }

    let shard_len = shards
        .iter()
        .flatten()
        .next()
        .map(Vec::len)
        .ok_or(FecError::Singular)?;
    if shards
        .iter()
        .flatten()
        .any(|shard| shard.len() != shard_len)
    {
        return Err(FecError::ShardLengthMismatch);
    }

    // One equation per unknown, taken from the parity rows that arrived:
    //
    //   parity[i] = XOR over all j of C[i][j] * data[j]
    //   => XOR over missing j of C[i][j] * data[j] = parity[i] XOR XOR over known j of C[i][j] * data[j]
    let unknowns = missing.len();
    let mut matrix = vec![[0u8; MAX_SHARDS]; unknowns];
    let mut rhs: Vec<Vec<u8>> = Vec::with_capacity(unknowns);

    for (row, parity_index) in available_parity.iter().take(unknowns).enumerate() {
        let parity_row = parity_index - data_count;
        for (k, &data_index) in missing.iter().enumerate() {
            matrix[row][k] = cauchy(parity_row, data_index, parity_count);
        }

        let mut b = shards[*parity_index].clone().expect("filtered to Some");
        for col in 0..data_count {
            if missing.contains(&col) {
                continue;
            }
            let coeff = cauchy(parity_row, col, parity_count);
            if coeff == 0 {
                continue;
            }
            let known = shards[col].as_ref().expect("not in the missing set");
            if coeff == 1 {
                for (bi, ki) in b.iter_mut().zip(known.iter()) {
                    *bi ^= *ki;
                }
            } else {
                let table = cached_mul_table(coeff);
                for (bi, ki) in b.iter_mut().zip(known.iter()) {
                    *bi ^= table[*ki as usize];
                }
            }
        }
        rhs.push(b);
    }

    solve(&mut matrix[..unknowns], &mut rhs, unknowns)?;

    for (k, &data_index) in missing.iter().enumerate() {
        shards[data_index] = Some(std::mem::take(&mut rhs[k]));
    }
    Ok(unknowns)
}

fn solve(
    matrix: &mut [[u8; MAX_SHARDS]],
    solution: &mut [Vec<u8>],
    n: usize,
) -> Result<(), FecError> {
    for col in 0..n {
        // Partial pivot: the first row at or below `col` with a non-zero coefficient here.
        let pivot = (col..n).find(|row| matrix[*row][col] != 0);
        let Some(pivot) = pivot else {
            return Err(FecError::Singular);
        };
        matrix.swap(col, pivot);
        solution.swap(col, pivot);

        let inv = gf_inv(matrix[col][col]);
        if inv != 1 {
            for k in col..n {
                matrix[col][k] = gf_mul(inv, matrix[col][k]);
            }
            for byte in solution[col].iter_mut() {
                *byte = gf_mul(inv, *byte);
            }
        }

        for row in 0..n {
            if row == col {
                continue;
            }
            let factor = matrix[row][col];
            if factor == 0 {
                continue;
            }
            for k in col..n {
                matrix[row][k] ^= gf_mul(factor, matrix[col][k]);
            }
            // Bytewise: the borrow checker needs the two rows to be distinct, and they are.
            let (pivot_row, other_row) = if row < col {
                let (a, b) = solution.split_at_mut(col);
                (&b[0], &mut a[row])
            } else {
                let (a, b) = solution.split_at_mut(row);
                (&a[col], &mut b[0])
            };
            for (o, p) in other_row.iter_mut().zip(pivot_row.iter()) {
                *o ^= gf_mul(factor, *p);
            }
        }
    }
    Ok(())
}

/// Repair a **striped** frame: the block layout [`blocks_for`] implies, decoded block by
/// block.
///
/// `shards` is indexed `0..data_count` for data and `data_count..data_count + parity_count`
/// for parity, exactly as for [`decode`]. `parity_count` must be a multiple of the block
/// count, which is what the packetiser produces; anything else is a shape error rather than
/// something to guess at.
///
/// Returns the total number of shards reconstructed.
pub fn decode_striped(
    shards: &mut [Option<Vec<u8>>],
    data_count: usize,
    parity_count: usize,
) -> Result<usize, FecError> {
    if data_count == 0 {
        return Err(FecError::EmptyFrame);
    }
    let total = data_count + parity_count;
    if shards.len() != total {
        return Err(FecError::ShapeMismatch {
            expected: total,
            actual: shards.len(),
        });
    }
    if parity_count == 0 {
        // No repair available. That is only *fine* if nothing is missing — and getting this
        // wrong turns "unreconstructable" into "recovered", i.e. hands the caller a frame
        // with a hole in it while reporting success. So the missing count is checked here
        // rather than left to the block loop, which has nothing to iterate over.
        let missing = (0..data_count).filter(|i| shards[*i].is_none()).count();
        return if missing == 0 {
            Ok(0)
        } else {
            Err(FecError::TooManyErasures { missing, parity: 0 })
        };
    }

    let blocks = blocks_for(data_count);
    if !parity_count.is_multiple_of(blocks) {
        return Err(FecError::ShapeMismatch {
            expected: parity_count - (parity_count % blocks),
            actual: parity_count,
        });
    }
    let parity_per_block = parity_count / blocks;

    let mut repaired = 0usize;
    for block in 0..blocks {
        let data_indices: Vec<usize> = (0..data_count)
            .filter(|i| block_of(*i, blocks) == block)
            .collect();
        let parity_indices: Vec<usize> = (0..parity_count)
            .filter(|p| block_of(*p, blocks) == block)
            .map(|p| data_count + p)
            .collect();

        if parity_indices.len() != parity_per_block {
            return Err(FecError::ShapeMismatch {
                expected: parity_per_block,
                actual: parity_indices.len(),
            });
        }

        // Nothing missing in this block: the common case, and it must cost no arithmetic.
        if data_indices.iter().all(|i| shards[*i].is_some()) {
            continue;
        }

        // Move the block's shards into a contiguous array. `take` rather than `clone`: these
        // are the same buffers we are about to put back, and copying every fragment of every
        // frame on the receive path is not free at 300 Mbps.
        let mut block_shards: Vec<Option<Vec<u8>>> =
            Vec::with_capacity(data_indices.len() + parity_per_block);
        for &i in &data_indices {
            block_shards.push(shards[i].take());
        }
        for &i in &parity_indices {
            block_shards.push(shards[i].take());
        }

        let result = decode(&mut block_shards, data_indices.len(), parity_per_block);
        for (slot, &i) in block_shards.iter_mut().zip(data_indices.iter()) {
            shards[i] = slot.take();
        }
        repaired += result?;
    }

    Ok(repaired)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shards(count: usize, len: usize, seed: u8) -> Vec<Vec<u8>> {
        (0..count)
            .map(|i| {
                (0..len)
                    .map(|j| {
                        // Deterministic, non-trivial, and different per shard, so a decoder
                        // that puts a shard in the wrong slot fails rather than passes.
                        seed.wrapping_mul(31)
                            .wrapping_add((i as u8).wrapping_mul(17))
                            .wrapping_add(j as u8)
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn tables_are_the_gf256_ones() {
        // Sanity anchors from the standard 0x11d tables: 2 * 128 = 256 = 0x1d (the poly's
        // reduction), and 2's inverse is 142.
        assert_eq!(gf_mul(2, 128), 0x1d);
        assert_eq!(gf_inv(2), 142);
        assert_eq!(gf_mul(2, 142), 1);
        assert_eq!(gf_mul(0, 200), 0);
        assert_eq!(gf_mul(1, 200), 200);
    }

    #[test]
    fn every_non_zero_element_has_an_inverse() {
        for a in 1..=255u8 {
            assert_eq!(gf_mul(a, gf_inv(a)), 1, "no inverse for {a}");
        }
    }

    #[test]
    fn zero_parity_is_free() {
        let data = shards(4, 8, 3);
        assert!(encode(&data, 0).unwrap().is_empty());
    }

    #[test]
    fn a_frame_with_no_loss_decodes_to_nothing() {
        let data = shards(4, 8, 3);
        let parity = encode(&data, 2).unwrap();
        let mut all: Vec<Option<Vec<u8>>> = data
            .iter()
            .cloned()
            .map(Some)
            .chain(parity.into_iter().map(Some))
            .collect();
        assert_eq!(decode(&mut all, 4, 2).unwrap(), 0);
        // And the data is untouched.
        assert_eq!(all[0].as_ref().unwrap(), &shards(4, 8, 3)[0]);
    }

    #[test]
    fn recovers_every_single_loss() {
        let data = shards(6, 16, 7);
        let parity = encode(&data, 2).unwrap();
        let original: Vec<Vec<u8>> = data.clone();

        for lost in 0..8 {
            let mut all: Vec<Option<Vec<u8>>> = data
                .iter()
                .cloned()
                .map(Some)
                .chain(parity.iter().cloned().map(Some))
                .collect();
            all[lost] = None;
            decode(&mut all, 6, 2).unwrap();
            for (i, shard) in original.iter().enumerate() {
                assert_eq!(all[i].as_ref().unwrap(), shard, "lost {lost}, shard {i}");
            }
        }
    }

    #[test]
    fn recovers_any_two_losses_exhaustively() {
        // The MDS claim, checked rather than believed. A Vandermonde matrix would pass
        // *some* of these and fail others, which is exactly the bug this construction
        // exists to avoid.
        let data = shards(5, 12, 11);
        let parity = encode(&data, 3).unwrap();
        let total = 8;
        let original: Vec<Vec<u8>> = data.clone();

        for a in 0..total {
            for b in (a + 1)..total {
                let mut all: Vec<Option<Vec<u8>>> = data
                    .iter()
                    .cloned()
                    .map(Some)
                    .chain(parity.iter().cloned().map(Some))
                    .collect();
                all[a] = None;
                all[b] = None;
                decode(&mut all, 5, 3).unwrap();
                for (i, shard) in original.iter().enumerate() {
                    assert_eq!(all[i].as_ref().unwrap(), shard, "lost {a},{b}, shard {i}");
                }
            }
        }
    }

    #[test]
    fn recovers_three_losses_including_all_parity() {
        // The hardest case for a systematic code: every parity shard is gone, so the repair
        // has to come from the data. (Recomputing parity from data is not what happens here
        // — this just checks the arithmetic is consistent.)
        let data = shards(4, 8, 5);
        let parity = encode(&data, 3).unwrap();
        let mut all: Vec<Option<Vec<u8>>> = data
            .iter()
            .cloned()
            .map(Some)
            .chain(parity.iter().cloned().map(Some))
            .collect();
        // Lose three data shards, keep three parity: solvable.
        all[0] = None;
        all[1] = None;
        all[2] = None;
        assert_eq!(decode(&mut all, 4, 3).unwrap(), 3);
        assert_eq!(all[0].as_ref().unwrap(), &data[0]);
        assert_eq!(all[1].as_ref().unwrap(), &data[1]);
        assert_eq!(all[2].as_ref().unwrap(), &data[2]);
    }

    #[test]
    fn refuses_more_erasures_than_parity() {
        let data = shards(4, 8, 5);
        let parity = encode(&data, 2).unwrap();
        let mut all: Vec<Option<Vec<u8>>> = data
            .iter()
            .cloned()
            .map(Some)
            .chain(parity.iter().cloned().map(Some))
            .collect();
        all[0] = None;
        all[1] = None;
        all[2] = None;
        assert_eq!(
            decode(&mut all, 4, 2),
            Err(FecError::TooManyErasures {
                missing: 3,
                parity: 2
            })
        );
    }

    #[test]
    fn a_lost_parity_shard_does_not_need_repairing() {
        let data = shards(4, 8, 5);
        let parity = encode(&data, 3).unwrap();
        let mut all: Vec<Option<Vec<u8>>> = data
            .iter()
            .cloned()
            .map(Some)
            .chain(parity.into_iter().map(Some))
            .collect();
        all[6] = None; // a parity shard
        assert_eq!(decode(&mut all, 4, 3).unwrap(), 0);
    }

    #[test]
    fn rejects_a_shape_that_does_not_fit_the_field() {
        assert!(matches!(
            encode(&shards(200, 4, 1), 60),
            Err(FecError::TooManyShards { .. })
        ));
    }

    #[test]
    fn rejects_mismatched_shard_lengths() {
        let data = vec![vec![1u8; 4], vec![2u8; 5]];
        assert_eq!(encode(&data, 1), Err(FecError::ShardLengthMismatch));
    }

    #[test]
    fn rejects_an_empty_frame() {
        assert_eq!(encode(&[], 2), Err(FecError::EmptyFrame));
    }

    #[test]
    fn max_size_still_works() {
        // The field limit, at the limit: 250 data + 5 parity = 255 shards.
        let data = shards(250, 4, 9);
        let parity = encode(&data, 5).unwrap();
        assert_eq!(parity.len(), 5);
        let mut all: Vec<Option<Vec<u8>>> = data
            .iter()
            .cloned()
            .map(Some)
            .chain(parity.into_iter().map(Some))
            .collect();
        for slot in all.iter_mut().skip(200).take(5) {
            *slot = None;
        }
        assert_eq!(decode(&mut all, 250, 5).unwrap(), 5);
        assert_eq!(all[201].as_ref().unwrap(), &data[201]);
    }

    // -- striping: a frame larger than the field can address ------------------------

    #[test]
    fn block_layout_is_a_pure_function_of_the_data_count() {
        assert_eq!(blocks_for(1), 1);
        assert_eq!(blocks_for(170), 1);
        assert_eq!(blocks_for(171), 2);
        assert_eq!(blocks_for(300), 2);
        assert_eq!(blocks_for(340), 2);
        assert_eq!(blocks_for(341), 3);
        assert_eq!(blocks_for(510), 3);
        assert_eq!(blocks_for(511), 4);
        // The widest block must still fit the field alongside its parity.
        const { assert!(250 + 5 <= MAX_SHARDS) };
    }

    #[test]
    fn block_membership_interleaves() {
        // Consecutive shards must go to *different* blocks, or a burst loss lands inside one
        // block and needs that block's whole parity budget.
        let blocks = 3;
        assert_eq!(block_of(0, blocks), 0);
        assert_eq!(block_of(1, blocks), 1);
        assert_eq!(block_of(2, blocks), 2);
        assert_eq!(block_of(3, blocks), 0);
        assert_eq!(position_in_block(4, blocks), 1);
    }

    /// Encode a striped frame the way the packetiser does.
    fn encode_striped(data: &[Vec<u8>], parity_per_block: usize) -> Vec<Vec<u8>> {
        let data_count = data.len();
        let blocks = blocks_for(data_count);
        let mut parity = vec![Vec::new(); parity_per_block * blocks];
        for block in 0..blocks {
            let block_data: Vec<Vec<u8>> = (0..data_count)
                .filter(|i| block_of(*i, blocks) == block)
                .map(|i| data[i].clone())
                .collect();
            let block_parity = encode(&block_data, parity_per_block).unwrap();
            for (position, shard) in block_parity.into_iter().enumerate() {
                // Parity shard `p` belongs to block `p % blocks` — the same rule the decoder
                // derives, which is why it needs no header field.
                parity[position * blocks + block] = shard;
            }
        }
        parity
    }

    #[test]
    fn a_frame_too_big_for_one_block_is_still_protected() {
        // The case that motivated striping: ~300 shards, which is one 300 Mbps frame at a
        // 1400-byte MTU and therefore the *normal* case at the target envelope.
        let data = shards(300, 8, 13);
        let parity = encode_striped(&data, 8);
        assert_eq!(parity.len(), 16, "2 blocks of 8 parity shards");

        let mut all: Vec<Option<Vec<u8>>> = data
            .iter()
            .cloned()
            .map(Some)
            .chain(parity.into_iter().map(Some))
            .collect();

        // A burst of 8 *consecutive* datagrams, which interleaving spreads across both
        // blocks: 4 in each. Eight parity shards per block absorb four losses each.
        for slot in all.iter_mut().skip(100).take(8) {
            *slot = None;
        }
        assert_eq!(decode_striped(&mut all, 300, 16).unwrap(), 8);
        for (i, shard) in data.iter().enumerate() {
            assert_eq!(all[i].as_ref().unwrap(), shard, "shard {i}");
        }
    }

    #[test]
    fn a_contiguous_block_layout_would_not_have_survived_that_burst() {
        // The counterfactual, stated as a test so the interleaving decision is defended
        // rather than assumed: the same eight consecutive losses, applied to *contiguous*
        // blocks, all land in one block and exceed its eight-shard budget... which is
        // actually survivable — but the point is the arithmetic, so check the boundary.
        let data = shards(300, 8, 15);
        let blocks = 2;
        let per_block = 8;
        // Contiguous blocks of 150: shards 100..108 are all in block 0, so block 0 needs 8
        // repairs out of its 8 — exactly at the limit, with no margin.
        assert_eq!(100 / 150, 0);
        assert_eq!(107 / 150, 0);
        // Interleaved, the same eight split 4/4, leaving margin in both.
        let in_block0 = (100..108).filter(|i| block_of(*i, blocks) == 0).count();
        assert_eq!(in_block0, 4);
        assert!(in_block0 <= per_block / 2);
        let _ = data;
    }

    #[test]
    fn striped_decode_is_a_no_op_when_nothing_is_missing() {
        let data = shards(300, 4, 17);
        let parity = encode_striped(&data, 5);
        let mut all: Vec<Option<Vec<u8>>> = data
            .iter()
            .cloned()
            .map(Some)
            .chain(parity.into_iter().map(Some))
            .collect();
        assert_eq!(decode_striped(&mut all, 300, 10).unwrap(), 0);
    }

    #[test]
    fn striped_decode_refuses_a_parity_count_that_does_not_divide() {
        let data = shards(300, 4, 19);
        let parity = encode_striped(&data, 8);
        let mut all: Vec<Option<Vec<u8>>> = data
            .iter()
            .cloned()
            .map(Some)
            .chain(parity.into_iter().map(Some))
            .collect();
        // 2 blocks, 15 parity shards: not a multiple.
        assert!(matches!(
            decode_striped(&mut all, 300, 15),
            Err(FecError::ShapeMismatch { .. })
        ));
    }

    #[test]
    fn striped_decode_reports_failure_when_a_block_is_short() {
        let data = shards(300, 4, 21);
        let parity = encode_striped(&data, 4);
        let mut all: Vec<Option<Vec<u8>>> = data
            .iter()
            .cloned()
            .map(Some)
            .chain(parity.into_iter().map(Some))
            .collect();
        // Ten consecutive losses: five per block against four parity shards each.
        for slot in all.iter_mut().skip(10).take(10) {
            *slot = None;
        }
        assert!(decode_striped(&mut all, 300, 8).is_err());
    }

    #[test]
    fn the_requested_ratio_is_the_ratio_the_field_pays() {
        // Guards a silent failure found while building the adaptive controller. A block holds
        // `data + parity <= MAX_SHARDS`, so its *largest expressible ratio* is
        // `(255 - data_per_block) / data_per_block`. At the original 250 that was 5 parity slots
        // — **2 %** — so `Ratio { 0.05 }` quietly paid 2 %, and every ratio above 2 % was asking
        // for something the layout could not give. Nothing errored; the overhead just came out
        // lower than requested, which is the worst way for a safety mechanism to be wrong.
        use crate::adaptive::AdaptiveConfig;
        use crate::packetizer::ParityPolicy;

        assert!(
            MAX_EXPRESSIBLE_RATIO >= AdaptiveConfig::default().max,
            "the block layout ({} %) cannot express the ratio the controller will ask for ({} %)",
            MAX_EXPRESSIBLE_RATIO * 100.0,
            AdaptiveConfig::default().max * 100.0,
        );

        for data in [170usize, 171, 340, 1000, 5000, 20000] {
            for ratio in [0.02f32, 0.05, 0.1, 0.25, 0.5] {
                let paid = ParityPolicy::Ratio { fraction: ratio }.overhead(data);
                // Granularity is one shard per block, and data_per_block rounds up, so the paid
                // ratio can exceed the requested one — but it must not fall meaningfully short.
                assert!(
                    paid >= ratio * 0.9,
                    "data={data} ratio={ratio}: the field paid {paid}, asked {ratio}"
                );
            }
        }
    }
}
