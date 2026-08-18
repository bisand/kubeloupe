//! Periodic snapshot of the store, so a restart does not cost a day of
//! history.
//!
//! Deliberately a snapshot rather than a write-through store: Lens
//! re-queries every chart once a minute over ranges up to 24h, so a store
//! that read from disk would be doing constant I/O to serve a working set
//! that fits in 9 MB of RAM. One sequential write every few minutes keeps
//! the query path a pointer walk and still bounds the loss to the
//! snapshot interval.
//!
//! Three properties matter more than the format:
//!
//! * **The write is atomic.** It goes to a temporary file and is renamed
//!   over the target. A process killed mid-write leaves the previous
//!   good snapshot untouched -- without this, an OOM during serialization
//!   would turn one crash into permanent data loss.
//! * **A bad snapshot is never fatal.** Load returns an error and the
//!   caller starts empty. Persistence that can wedge the daemon into
//!   CrashLoopBackOff is worse than no persistence.
//! * **Nothing is buffered whole in memory.** It streams through a
//!   BufWriter, because a snapshot taken to survive OOM must not itself
//!   be a memory spike.

use crate::store::{Labels, Sample, Store};
use anyhow::{Context, Result, bail};
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

// "LensMetricsD v1", from before the rename. Deliberately left alone: the
// magic identifies the on-disk format, not the project, and changing it
// would make every existing snapshot unreadable -- which the loader
// handles gracefully, by starting empty and throwing away the day of
// history it was written to preserve.
const MAGIC: &[u8; 4] = b"LMD1";
const VERSION: u16 = 1;

/// Refuse absurd values rather than allocating from a corrupt header.
const MAX_SERIES: u32 = 1_000_000;
const MAX_SAMPLES_PER_SERIES: u32 = 10_000_000;
const MAX_LABELS: u16 = 256;
const MAX_STRING: u16 = 4096;

pub fn save(store: &Store, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    // Same directory, so the rename below is within one filesystem and
    // therefore atomic.
    let tmp = path.with_extension("tmp");
    {
        let file = File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        let mut out = BufWriter::new(file);

        out.write_all(MAGIC)?;
        out.write_all(&VERSION.to_le_bytes())?;

        let count: u32 = store.series_count().try_into().unwrap_or(u32::MAX);
        out.write_all(&count.to_le_bytes())?;

        for series in store.series_iter() {
            let labels: u16 = series.labels.len().try_into().unwrap_or(u16::MAX);
            out.write_all(&labels.to_le_bytes())?;
            for (key, value) in &series.labels {
                write_string(&mut out, key)?;
                write_string(&mut out, value)?;
            }

            let samples: u32 = series.samples.len().try_into().unwrap_or(u32::MAX);
            out.write_all(&samples.to_le_bytes())?;
            for sample in &series.samples {
                out.write_all(&sample.t.to_le_bytes())?;
                out.write_all(&sample.v.to_le_bytes())?;
            }
        }

        out.flush()?;
        // Without this the rename can be durable while the contents are
        // not, which on a hard power loss is exactly the truncated file
        // the temp-and-rename dance exists to avoid.
        out.into_inner().map_err(|e| e.into_error())?.sync_all()?;
    }

    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Restores a store, dropping anything already older than `retention` at
/// `now` -- a pod that was down for six hours should not come back with
/// six hours of stale points.
pub fn load(path: &Path, retention: i64, now: i64) -> Result<Store> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut input = BufReader::new(file);

    let mut magic = [0u8; 4];
    input.read_exact(&mut magic).context("reading the header")?;
    if &magic != MAGIC {
        bail!("not a kubeloupe snapshot");
    }
    let version = read_u16(&mut input)?;
    if version != VERSION {
        bail!("snapshot version {version} is not supported");
    }

    let series_count = read_u32(&mut input)?;
    if series_count > MAX_SERIES {
        bail!("implausible series count {series_count}");
    }

    let cutoff = now - retention;
    let mut store = Store::new(retention);

    for _ in 0..series_count {
        let label_count = read_u16(&mut input)?;
        if label_count > MAX_LABELS {
            bail!("implausible label count {label_count}");
        }
        let mut labels = Labels::new();
        for _ in 0..label_count {
            let key = read_string(&mut input)?;
            let value = read_string(&mut input)?;
            labels.insert(key, value);
        }

        let sample_count = read_u32(&mut input)?;
        if sample_count > MAX_SAMPLES_PER_SERIES {
            bail!("implausible sample count {sample_count}");
        }
        let mut samples = Vec::with_capacity(sample_count as usize);
        for _ in 0..sample_count {
            let t = read_i64(&mut input)?;
            let v = read_f64(&mut input)?;
            // Timestamps are ascending in the file; the binary searches in
            // the store rely on that, so a rewound clock must not be able
            // to interleave them.
            if t >= cutoff && samples.last().is_none_or(|last: &Sample| last.t < t) {
                samples.push(Sample { t, v });
            }
        }

        store.insert_series(labels, samples);
    }

    Ok(store)
}

fn write_string(out: &mut impl Write, value: &str) -> Result<()> {
    let bytes = value.as_bytes();
    let len: u16 = bytes.len().try_into().unwrap_or(u16::MAX);
    out.write_all(&len.to_le_bytes())?;
    out.write_all(&bytes[..len as usize])?;
    Ok(())
}

fn read_string(input: &mut impl Read) -> Result<String> {
    let len = read_u16(input)?;
    if len > MAX_STRING {
        bail!("implausible string length {len}");
    }
    let mut buf = vec![0u8; len as usize];
    input.read_exact(&mut buf)?;
    String::from_utf8(buf).context("a label was not valid UTF-8")
}

macro_rules! read_le {
    ($name:ident, $ty:ty, $n:expr) => {
        fn $name(input: &mut impl Read) -> Result<$ty> {
            let mut buf = [0u8; $n];
            input
                .read_exact(&mut buf)
                .context("unexpected end of snapshot")?;
            Ok(<$ty>::from_le_bytes(buf))
        }
    };
}

read_le!(read_u16, u16, 2);
read_le!(read_u32, u32, 4);
read_le!(read_i64, i64, 8);
read_le!(read_f64, f64, 8);
