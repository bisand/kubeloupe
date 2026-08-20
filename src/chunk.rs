//! Compressed, immutable blocks of samples.
//!
//! A raw `{i64, f64}` sample costs 16 bytes, and almost every one of them
//! is redundant: the collector scrapes on a fixed interval, so consecutive
//! timestamps differ by exactly the same amount, and the values are
//! integers wearing a float costume -- bytes, nanoseconds and millicores
//! that were divided into `f64` on the way in.
//!
//! So a sealed chunk stores two varint streams instead: timestamp deltas,
//! and delta-of-deltas of the values recovered as integers. Measured over
//! 24h of a 673-series cluster that is **1.3 bytes per sample**, a little
//! over twelve times smaller than the raw form.
//!
//! Everything here is byte-aligned. Bit-packing the streams the way
//! Gorilla does would save perhaps another 0.15 bytes per sample, which
//! is not worth the code it takes to read.
//!
//! ## Losslessness
//!
//! The integer path is only taken when it round-trips every value in the
//! chunk bit-for-bit; `encode` checks before committing to it and falls
//! back to raw `f64` otherwise. A chart that quietly loses the low bits
//! of a counter is worse than one that costs 8 bytes a sample.

use anyhow::{Result, bail};

use crate::store::Sample;

/// Stream layouts. Constant series -- resource limits, node capacity --
/// collapse under run-length encoding, while noisy ones are smaller as
/// plain varints. Both are cheap to produce, so `encode_stream` writes
/// whichever came out shorter and records which it picked.
const STREAM_PLAIN: u8 = 0;
const STREAM_RLE: u8 = 1;

/// Value layouts.
const VALUES_INT: u8 = 0;
const VALUES_RAW: u8 = 1;

/// The largest power of ten tried when recovering integers. Nanosecond
/// counters divided by 1e9 need all nine.
const MAX_SCALE: u32 = 9;

/// Ceiling on how much is reserved from a length that was read rather than
/// computed. Comfortably above a real chunk, so this costs nothing in
/// practice and bounds what a corrupt header can ask for.
const CAPACITY_HINT: usize = 4_096;

/// One sealed run of samples. Immutable once built: retention drops whole
/// chunks rather than rewriting them.
pub struct Chunk {
    first_t: i64,
    last_t: i64,
    count: u32,
    bytes: Box<[u8]>,
}

impl Chunk {
    pub fn first_t(&self) -> i64 {
        self.first_t
    }

    pub fn last_t(&self) -> i64 {
        self.last_t
    }

    pub fn len(&self) -> usize {
        self.count as usize
    }

    /// Bytes actually held, for the sizing tests.
    #[cfg(test)]
    pub fn heap_size(&self) -> usize {
        self.bytes.len()
    }

    /// Restores a chunk from a snapshot without re-encoding it.
    pub fn from_parts(first_t: i64, last_t: i64, count: u32, bytes: Box<[u8]>) -> Self {
        Self {
            first_t,
            last_t,
            count,
            bytes,
        }
    }

    pub fn raw_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn count(&self) -> u32 {
        self.count
    }
}

/// Compresses `samples`, which must be non-empty and ascending by
/// timestamp.
pub fn encode(samples: &[Sample]) -> Chunk {
    debug_assert!(!samples.is_empty());
    debug_assert!(samples.windows(2).all(|w| w[0].t < w[1].t));

    let mut bytes = Vec::new();

    let deltas: Vec<i64> = samples.windows(2).map(|w| w[1].t - w[0].t).collect();
    encode_stream(&mut bytes, &deltas);

    match int_stream(samples) {
        Some((scale, first, dods)) => {
            bytes.push(VALUES_INT);
            bytes.push(scale as u8);
            put_uvarint(&mut bytes, zigzag(first));
            encode_stream(&mut bytes, &dods);
        }
        None => {
            bytes.push(VALUES_RAW);
            for sample in samples {
                bytes.extend_from_slice(&sample.v.to_le_bytes());
            }
        }
    }

    Chunk {
        first_t: samples[0].t,
        last_t: samples[samples.len() - 1].t,
        count: samples.len() as u32,
        bytes: bytes.into_boxed_slice(),
    }
}

/// Expands a chunk back into samples, appending to `out`.
pub fn decode_into(chunk: &Chunk, out: &mut Vec<Sample>) -> Result<()> {
    let mut input = &chunk.bytes[..];
    let n = chunk.count as usize;

    let deltas = decode_stream(&mut input, n.saturating_sub(1))?;
    let mut times = Vec::with_capacity(n.min(CAPACITY_HINT));
    let mut t = chunk.first_t;
    times.push(t);
    for d in &deltas {
        t = t
            .checked_add(*d)
            .ok_or_else(|| anyhow::anyhow!("timestamp overflow in a chunk"))?;
        times.push(t);
    }

    let mode = take_u8(&mut input)?;
    match mode {
        VALUES_INT => {
            let scale = take_u8(&mut input)? as u32;
            if scale > MAX_SCALE {
                bail!("chunk scale {scale} out of range");
            }
            let divisor = 10f64.powi(scale as i32);
            let first = unzigzag(get_uvarint(&mut input)?);
            let dods = decode_stream(&mut input, n.saturating_sub(1))?;

            let mut value = first;
            let mut delta = 0i64;
            out.push(Sample {
                t: times[0],
                v: value as f64 / divisor,
            });
            for (i, dod) in dods.iter().enumerate() {
                delta = delta
                    .checked_add(*dod)
                    .ok_or_else(|| anyhow::anyhow!("value overflow in a chunk"))?;
                value = value
                    .checked_add(delta)
                    .ok_or_else(|| anyhow::anyhow!("value overflow in a chunk"))?;
                out.push(Sample {
                    t: times[i + 1],
                    v: value as f64 / divisor,
                });
            }
        }
        VALUES_RAW => {
            for time in times.iter().take(n) {
                if input.len() < 8 {
                    bail!("chunk ended mid-value");
                }
                let (head, rest) = input.split_at(8);
                input = rest;
                out.push(Sample {
                    t: *time,
                    v: f64::from_le_bytes(head.try_into().unwrap()),
                });
            }
        }
        other => bail!("unknown chunk value mode {other}"),
    }

    Ok(())
}

/// Recovers the samples as scaled integers, but only if doing so is
/// lossless. Returns the scale, the first value, and the delta-of-delta
/// stream.
fn int_stream(samples: &[Sample]) -> Option<(u32, i64, Vec<i64>)> {
    'scale: for scale in 0..=MAX_SCALE {
        let factor = 10f64.powi(scale as i32);
        let mut ints = Vec::with_capacity(samples.len());

        for sample in samples {
            let scaled = sample.v * factor;
            // `as i64` saturates rather than wrapping, but a value this
            // large is not going to be compressible anyway.
            if !scaled.is_finite() || scaled.abs() >= 9e18 {
                return None;
            }
            let rounded = scaled.round();
            // The only test that matters: does it come back identical?
            // Compared as bits rather than values, because `-0.0 == 0.0`
            // is true and would let negative zero through as positive.
            // NaN never survives this either, which is correct -- it
            // belongs in the raw path.
            if ((rounded as i64) as f64 / factor).to_bits() != sample.v.to_bits() {
                continue 'scale;
            }
            ints.push(rounded as i64);
        }

        let mut dods = Vec::with_capacity(ints.len().saturating_sub(1));
        let mut prev_delta = 0i64;
        for pair in ints.windows(2) {
            let delta = pair[1].checked_sub(pair[0])?;
            dods.push(delta.checked_sub(prev_delta)?);
            prev_delta = delta;
        }
        return Some((scale, ints[0], dods));
    }
    None
}

fn encode_stream(out: &mut Vec<u8>, values: &[i64]) {
    let mut plain = Vec::with_capacity(values.len());
    for v in values {
        put_uvarint(&mut plain, zigzag(*v));
    }

    let mut rle = Vec::new();
    let mut runs = 0u64;
    let mut i = 0;
    while i < values.len() {
        let mut run = 1usize;
        while i + run < values.len() && values[i + run] == values[i] {
            run += 1;
        }
        put_uvarint(&mut rle, zigzag(values[i]));
        put_uvarint(&mut rle, run as u64);
        runs += 1;
        i += run;
    }
    let mut rle_framed = Vec::with_capacity(rle.len() + 4);
    put_uvarint(&mut rle_framed, runs);
    rle_framed.extend_from_slice(&rle);

    if rle_framed.len() < plain.len() {
        out.push(STREAM_RLE);
        out.extend_from_slice(&rle_framed);
    } else {
        out.push(STREAM_PLAIN);
        out.extend_from_slice(&plain);
    }
}

fn decode_stream(input: &mut &[u8], expected: usize) -> Result<Vec<i64>> {
    let mode = take_u8(input)?;
    // `expected` comes from a chunk header, which on the load path comes
    // from a file. Grow into it rather than trusting it up front.
    let mut out = Vec::with_capacity(expected.min(CAPACITY_HINT));
    match mode {
        STREAM_PLAIN => {
            for _ in 0..expected {
                out.push(unzigzag(get_uvarint(input)?));
            }
        }
        STREAM_RLE => {
            let runs = get_uvarint(input)?;
            for _ in 0..runs {
                let value = unzigzag(get_uvarint(input)?);
                let count = get_uvarint(input)?;
                if out.len() as u64 + count > expected as u64 {
                    bail!("run-length stream is longer than its chunk");
                }
                for _ in 0..count {
                    out.push(value);
                }
            }
            if out.len() != expected {
                bail!("run-length stream is shorter than its chunk");
            }
        }
        other => bail!("unknown chunk stream mode {other}"),
    }
    Ok(out)
}

fn take_u8(input: &mut &[u8]) -> Result<u8> {
    match input.split_first() {
        Some((first, rest)) => {
            *input = rest;
            Ok(*first)
        }
        None => bail!("chunk ended early"),
    }
}

fn put_uvarint(out: &mut Vec<u8>, mut n: u64) {
    while n >= 0x80 {
        out.push((n as u8) | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
}

fn get_uvarint(input: &mut &[u8]) -> Result<u64> {
    let mut result = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = take_u8(input)?;
        if shift >= 64 {
            bail!("varint is too long to be valid");
        }
        result |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Ok(result);
        }
        shift += 7;
    }
}

fn zigzag(n: i64) -> u64 {
    (n.wrapping_shl(1) ^ (n >> 63)) as u64
}

fn unzigzag(n: u64) -> i64 {
    ((n >> 1) as i64) ^ -((n & 1) as i64)
}
