//! The disk tier: history kept in files rather than memory, for as long as
//! `STORAGE_RETENTION_DAYS` says. Off unless `STORAGE_DIR` is set; without
//! it everything stays in memory, as it always has.
//!
//! There is no database. The memory store already packs samples into
//! immutable chunks (see [`crate::chunk`]), and immutable chunks are what
//! a time series database writes anyway. So once an hour every series is
//! sealed and its chunks are appended to the day's file as one segment,
//! and memory is left holding the hour in progress. Retention deletes
//! whole days; nothing is ever rewritten in place.
//!
//! A day file is a run of segments:
//!
//! ```text
//! header   "KLM1" resolution:u32 min_t:i64 max_t:i64
//!          data_len:u32 names_len:u32 entries_len:u32
//! data     chunk bytes, each series' contiguous
//! names    count:u32, then per metric name: len:u16 name
//!          entries_offset:u32 entries_len:u32
//! entries  per series: len:u16 labels (as the store's key, less the
//!          name) chunks:u32, then per chunk: first_t:i64 last_t:i64
//!          count:u32 offset:u32 len:u32
//! ```
//!
//! A query reads the small name table of each segment in its range, then
//! the entries of the names it asks for, then only the chunks that overlap
//! it -- with `pread`, so the page cache does the caching.
//!
//! Once a day is over its segments are merged into one, each series' hours
//! packed into a single chunk, and once it is
//! older than `STORAGE_DOWNSAMPLE_AFTER_DAYS` it is rewritten keeping one
//! point per `STORAGE_DOWNSAMPLE_SECONDS` -- the last, which is what a
//! counter needs for `rate()` and `increase()` to stay exact.
//!
//! A segment is written whole with one `write`, so a crash can leave only
//! a torn tail, which loading truncates away.

use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::chunk::{self, Chunk};
use crate::store::{Labels, NAME_LABEL, Sample};

const MAGIC: &[u8; 4] = b"KLM1";
const HEADER: u64 = 36;
const DAY: i64 = 86_400;
/// Bounds on what a corrupt header may ask to be read.
const MAX_SECTION: u64 = 1 << 30;

pub struct Config {
    pub dir: PathBuf,
    /// Seconds.
    pub retention: i64,
    pub max_bytes: u64,
    /// Seconds; `None` keeps every sample for the whole retention.
    pub downsample_after: Option<i64>,
    /// Seconds between the points kept once downsampled.
    pub resolution: i64,
}

impl Config {
    pub fn from_env() -> Option<Self> {
        let dir = std::env::var("STORAGE_DIR")
            .ok()
            .filter(|d| !d.trim().is_empty())?;
        let num = |key: &str, default: i64| -> i64 {
            std::env::var(key)
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(default)
        };
        Some(Self {
            dir: PathBuf::from(dir),
            retention: num("STORAGE_RETENTION_DAYS", 30).max(1) * DAY,
            max_bytes: (num("STORAGE_MAX_MB", 1024).max(1) as u64) << 20,
            downsample_after: std::env::var("STORAGE_DOWNSAMPLE_AFTER_DAYS")
                .ok()
                .and_then(|v| v.trim().parse::<i64>().ok())
                .map(|d| d.max(1) * DAY),
            resolution: num("STORAGE_DOWNSAMPLE_SECONDS", 300).max(1),
        })
    }
}

struct Segment {
    /// File offset of the header.
    at: u64,
    resolution: i64,
    min_t: i64,
    max_t: i64,
    data_len: u64,
    names_len: u64,
    entries_len: u64,
}

impl Segment {
    fn data(&self) -> u64 {
        self.at + HEADER
    }
    fn names(&self) -> u64 {
        self.data() + self.data_len
    }
    fn entries(&self) -> u64 {
        self.names() + self.names_len
    }
    fn end(&self) -> u64 {
        self.entries() + self.entries_len
    }
}

struct Day {
    day: i64,
    path: PathBuf,
    file: File,
    segments: Vec<Segment>,
}

impl Day {
    fn size(&self) -> u64 {
        self.segments.last().map_or(0, Segment::end)
    }
    fn min_t(&self) -> i64 {
        self.segments
            .iter()
            .map(|s| s.min_t)
            .min()
            .unwrap_or(i64::MAX)
    }
    fn max_t(&self) -> i64 {
        self.segments
            .iter()
            .map(|s| s.max_t)
            .max()
            .unwrap_or(i64::MIN)
    }
}

pub struct Disk {
    pub config: Config,
    /// Oldest first.
    days: Vec<Day>,
}

/// One series' chunks, as read from a segment.
struct Entry {
    labels: Labels,
    /// first_t, last_t, count, absolute offset, len.
    chunks: Vec<(i64, i64, u32, u64, u32)>,
}

impl Disk {
    /// Opens the directory, creating it, and reads every day's segment
    /// headers. A torn last segment is cut off rather than failing.
    pub fn open(config: Config) -> Result<Self> {
        std::fs::create_dir_all(&config.dir)
            .with_context(|| format!("creating {}", config.dir.display()))?;
        let mut days = Vec::new();
        for entry in std::fs::read_dir(&config.dir)?.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".tmp") {
                let _ = std::fs::remove_file(entry.path());
                continue;
            }
            let Some(day) = name
                .strip_suffix(".klm")
                .and_then(|d| crate::logs::parse_rfc3339(&format!("{d}T00:00:00Z")))
                .map(|ns| ns / 1_000_000_000 / DAY)
            else {
                continue;
            };
            match load_day(day, entry.path()) {
                Ok(day) => days.push(day),
                Err(error) => eprintln!("kubeloupe: skipping {name}: {error:#}"),
            }
        }
        days.sort_by_key(|d| d.day);
        Ok(Self { config, days })
    }

    pub fn bytes(&self) -> u64 {
        self.days.iter().map(Day::size).sum()
    }

    pub fn days(&self) -> usize {
        self.days.len()
    }

    /// The coarsest resolution of anything newer than `t`: 0 when all of
    /// it is raw.
    pub fn resolution_since(&self, t: i64) -> i64 {
        self.days
            .iter()
            .flat_map(|d| &d.segments)
            .filter(|s| s.max_t >= t)
            .map(|s| s.resolution)
            .max()
            .unwrap_or(0)
    }

    /// Appends one segment, every series given with its chunks, to the
    /// file of the day its oldest sample is from: the hour flushed at
    /// midnight, which holds the first sample or two of the new day too,
    /// belongs to the day it was collected. Nothing is written for an
    /// empty hour.
    pub fn append(&mut self, series: &[(&Labels, &[Chunk])]) -> Result<()> {
        let mut by_name: BTreeMap<&str, Vec<(String, Vec<&Chunk>)>> = BTreeMap::new();
        for (labels, chunks) in series {
            if chunks.is_empty() {
                continue;
            }
            let name = labels.get(NAME_LABEL).map(String::as_str).unwrap_or("");
            by_name
                .entry(name)
                .or_default()
                .push((rest_key(labels), chunks.iter().collect()));
        }
        if by_name.is_empty() {
            return Ok(());
        }
        let mut builder = Builder::new();
        let mut data = Vec::new();
        for (name, series) in &by_name {
            builder.add(name, series, &mut data)?;
        }
        let (header, tail) = builder.finish(0)?;
        let mut segment = header;
        segment.extend_from_slice(&data);
        segment.extend_from_slice(&tail);
        let oldest = series
            .iter()
            .flat_map(|(_, chunks)| chunks.iter().map(Chunk::first_t))
            .min()
            .unwrap_or_default();
        let day = oldest.div_euclid(DAY);
        let index = match self.days.iter().position(|d| d.day == day) {
            Some(i) => i,
            None => {
                let path = self.path(day);
                let file = OpenOptions::new()
                    .create(true)
                    .read(true)
                    .append(true)
                    .open(&path)
                    .with_context(|| format!("opening {}", path.display()))?;
                self.days.push(Day {
                    day,
                    path,
                    file,
                    segments: Vec::new(),
                });
                self.days.sort_by_key(|d| d.day);
                self.days.iter().position(|d| d.day == day).unwrap_or(0)
            }
        };
        let target = &mut self.days[index];
        let at = target.size();
        // Append mode writes at the end of the file, which is `at` unless a
        // torn segment is still there: cut it first.
        target.file.set_len(at)?;
        (&target.file).write_all(&segment)?;
        target.file.sync_data()?;
        target.segments.push(parse_header(&segment, at)?);
        Ok(())
    }

    /// Every series matching `wanted`, with its samples in `[from, to]`
    /// (or none, when `samples` is false), merged into `out` by key.
    pub fn read(
        &self,
        name: &dyn Fn(&str) -> bool,
        wanted: &dyn Fn(&Labels) -> bool,
        from: i64,
        to: i64,
        samples: bool,
        out: &mut HashMap<String, (Labels, Vec<Sample>)>,
    ) {
        for day in &self.days {
            if day.max_t() < from || day.min_t() > to {
                continue;
            }
            for segment in &day.segments {
                if segment.max_t < from || segment.min_t > to {
                    continue;
                }
                if let Err(error) =
                    read_segment(&day.file, segment, name, wanted, from, to, samples, out)
                {
                    eprintln!(
                        "kubeloupe: reading {} at {}: {error:#}",
                        day.path.display(),
                        segment.at
                    );
                }
            }
        }
    }

    /// Retention, the size cap, merging finished days and downsampling old
    /// ones. Each rewrite goes to a temporary file renamed into place, so a
    /// crash leaves either the old day or the new one.
    pub fn maintain(&mut self, now: i64) {
        let today = now.div_euclid(DAY);
        let cutoff = now - self.config.retention;
        self.days.retain(|day| {
            let gone = day.max_t() < cutoff && day.day < today;
            if gone && std::fs::remove_file(&day.path).is_ok() {
                eprintln!("kubeloupe: metrics of {} aged out", day_name(day.day));
                return false;
            }
            true
        });
        while self.bytes() > self.config.max_bytes
            && self.days.first().is_some_and(|d| d.day < today)
        {
            let day = self.days.remove(0);
            let _ = std::fs::remove_file(&day.path);
            eprintln!(
                "kubeloupe: metrics of {} dropped to stay under STORAGE_MAX_MB",
                day_name(day.day)
            );
        }

        for i in 0..self.days.len() {
            let day = &self.days[i];
            if day.day >= today {
                continue;
            }
            let downsample = self
                .config
                .downsample_after
                .is_some_and(|after| day.max_t() < now - after)
                && day
                    .segments
                    .iter()
                    .any(|s| s.resolution < self.config.resolution);
            if day.segments.len() < 2 && !downsample {
                continue;
            }
            let resolution = if downsample {
                self.config.resolution
            } else {
                0
            };
            match rewrite(day, resolution) {
                Ok(replaced) => self.days[i] = replaced,
                Err(error) => eprintln!(
                    "kubeloupe: compacting {} failed: {error:#}",
                    day.path.display()
                ),
            }
        }
    }

    fn path(&self, day: i64) -> PathBuf {
        self.config.dir.join(format!("{}.klm", day_name(day)))
    }
}

fn day_name(day: i64) -> String {
    crate::logs::rfc3339(day * DAY * 1_000_000_000, 0)[..10].to_string()
}

fn load_day(day: i64, path: PathBuf) -> Result<Day> {
    let file = OpenOptions::new().read(true).append(true).open(&path)?;
    let len = file.metadata()?.len();
    let mut segments = Vec::new();
    let mut at = 0;
    while at < len {
        let mut header = [0u8; HEADER as usize];
        let segment = file
            .read_exact_at(&mut header, at)
            .ok()
            .and_then(|()| parse_header(&header, at).ok())
            .filter(|s| s.end() <= len);
        match segment {
            Some(segment) => {
                at = segment.end();
                segments.push(segment);
            }
            None => {
                eprintln!(
                    "kubeloupe: {} ends in a torn segment at {at}; cutting it off",
                    path.display()
                );
                file.set_len(at)?;
                break;
            }
        }
    }
    Ok(Day {
        day,
        path,
        file,
        segments,
    })
}

fn parse_header(bytes: &[u8], at: u64) -> Result<Segment> {
    if bytes.len() < HEADER as usize || &bytes[..4] != MAGIC {
        bail!("not a segment");
    }
    let u32_at = |i: usize| u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap_or_default());
    let i64_at = |i: usize| i64::from_le_bytes(bytes[i..i + 8].try_into().unwrap_or_default());
    let segment = Segment {
        at,
        resolution: i64::from(u32_at(4)),
        min_t: i64_at(8),
        max_t: i64_at(16),
        data_len: u64::from(u32_at(24)),
        names_len: u64::from(u32_at(28)),
        entries_len: u64::from(u32_at(32)),
    };
    if segment.names_len < 4 || segment.min_t > segment.max_t {
        bail!("inconsistent segment header");
    }
    Ok(segment)
}

/// The store's key without the name: the name is the table it sits under.
fn rest_key(labels: &Labels) -> String {
    let mut key = String::new();
    for (k, v) in labels {
        if k == NAME_LABEL {
            continue;
        }
        key.push_str(k);
        key.push('\u{1}');
        key.push_str(v);
        key.push('\u{2}');
    }
    key
}

fn labels_of(name: &str, key: &str) -> Labels {
    let mut labels = Labels::new();
    labels.insert(NAME_LABEL.to_string(), name.to_string());
    for pair in key.split('\u{2}').filter(|p| !p.is_empty()) {
        if let Some((k, v)) = pair.split_once('\u{1}') {
            labels.insert(k.to_string(), v.to_string());
        }
    }
    labels
}

/// Chunks to append, as stored bytes with their times and count.
trait Encoded {
    fn parts(&self) -> (i64, i64, u32, &[u8]);
}

impl Encoded for &Chunk {
    fn parts(&self) -> (i64, i64, u32, &[u8]) {
        (
            self.first_t(),
            self.last_t(),
            self.count(),
            self.raw_bytes(),
        )
    }
}

impl Encoded for Chunk {
    fn parts(&self) -> (i64, i64, u32, &[u8]) {
        (
            self.first_t(),
            self.last_t(),
            self.count(),
            self.raw_bytes(),
        )
    }
}

/// Builds one segment a metric name at a time. The chunk bytes go out as
/// each name is added; only the name table and entries, a few bytes a
/// series, are held until the end.
#[derive(Default)]
struct Builder {
    data_len: usize,
    names: Vec<u8>,
    count: u32,
    entries: Vec<u8>,
    min_t: i64,
    max_t: i64,
}

impl Builder {
    fn new() -> Self {
        Self {
            min_t: i64::MAX,
            max_t: i64::MIN,
            ..Self::default()
        }
    }

    /// Adds a name's series, appending their chunk bytes to `data`.
    fn add<C: Encoded>(
        &mut self,
        name: &str,
        series: &[(String, Vec<C>)],
        data: &mut Vec<u8>,
    ) -> Result<()> {
        let start = self.entries.len();
        for (key, chunks) in series {
            if chunks.is_empty() {
                continue;
            }
            put_str(&mut self.entries, key)?;
            self.entries
                .extend_from_slice(&(chunks.len() as u32).to_le_bytes());
            for chunk in chunks {
                let (first, last, count, bytes) = chunk.parts();
                self.min_t = self.min_t.min(first);
                self.max_t = self.max_t.max(last);
                self.entries.extend_from_slice(&first.to_le_bytes());
                self.entries.extend_from_slice(&last.to_le_bytes());
                self.entries.extend_from_slice(&count.to_le_bytes());
                self.entries
                    .extend_from_slice(&u32::try_from(self.data_len)?.to_le_bytes());
                self.entries
                    .extend_from_slice(&u32::try_from(bytes.len())?.to_le_bytes());
                data.extend_from_slice(bytes);
                self.data_len += bytes.len();
            }
        }
        if self.entries.len() > start {
            put_str(&mut self.names, name)?;
            self.names
                .extend_from_slice(&u32::try_from(start)?.to_le_bytes());
            self.names
                .extend_from_slice(&u32::try_from(self.entries.len() - start)?.to_le_bytes());
            self.count += 1;
        }
        Ok(())
    }

    /// The header, and what follows the data: the name table and entries.
    fn finish(self, resolution: i64) -> Result<(Vec<u8>, Vec<u8>)> {
        let mut tail = Vec::with_capacity(4 + self.names.len() + self.entries.len());
        tail.extend_from_slice(&self.count.to_le_bytes());
        tail.extend_from_slice(&self.names);
        let names_len = tail.len();
        tail.extend_from_slice(&self.entries);

        let mut header = Vec::with_capacity(HEADER as usize);
        header.extend_from_slice(MAGIC);
        header.extend_from_slice(&u32::try_from(resolution)?.to_le_bytes());
        header.extend_from_slice(&self.min_t.to_le_bytes());
        header.extend_from_slice(&self.max_t.to_le_bytes());
        header.extend_from_slice(&u32::try_from(self.data_len)?.to_le_bytes());
        header.extend_from_slice(&u32::try_from(names_len)?.to_le_bytes());
        header.extend_from_slice(&u32::try_from(self.entries.len())?.to_le_bytes());
        Ok((header, tail))
    }
}

fn put_str(out: &mut Vec<u8>, s: &str) -> Result<()> {
    out.extend_from_slice(&u16::try_from(s.len())?.to_le_bytes());
    out.extend_from_slice(s.as_bytes());
    Ok(())
}

fn read_at(file: &File, at: u64, len: u64) -> Result<Vec<u8>> {
    if len > MAX_SECTION {
        bail!("a section of {len} bytes is not believable");
    }
    let mut buf = vec![0u8; len as usize];
    file.read_exact_at(&mut buf, at)?;
    Ok(buf)
}

/// A cursor over a section read from disk; every read is bounds-checked,
/// so a corrupt file is an error, never a panic.
struct Cursor<'a>(&'a [u8]);

impl Cursor<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8]> {
        if self.0.len() < n {
            bail!("truncated section");
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into()?))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into()?))
    }
    fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into()?))
    }
    fn str(&mut self) -> Result<String> {
        let len = self.u16()? as usize;
        Ok(std::str::from_utf8(self.take(len)?)?.to_string())
    }
}

/// The name table: each name with where its entries are.
fn read_names(file: &File, segment: &Segment) -> Result<Vec<(String, u64, u64)>> {
    let bytes = read_at(file, segment.names(), segment.names_len)?;
    let mut cursor = Cursor(&bytes);
    let count = cursor.u32()?;
    let mut out = Vec::new();
    for _ in 0..count {
        let name = cursor.str()?;
        let offset = u64::from(cursor.u32()?);
        let len = u64::from(cursor.u32()?);
        if offset + len > segment.entries_len {
            bail!("entries of {name} run past the segment");
        }
        out.push((name, offset, len));
    }
    Ok(out)
}

fn read_entries(
    file: &File,
    segment: &Segment,
    name: &str,
    offset: u64,
    len: u64,
) -> Result<Vec<Entry>> {
    let bytes = read_at(file, segment.entries() + offset, len)?;
    let mut cursor = Cursor(&bytes);
    let mut out = Vec::new();
    while !cursor.0.is_empty() {
        let labels = labels_of(name, &cursor.str()?);
        let count = cursor.u32()?;
        let mut chunks = Vec::new();
        for _ in 0..count {
            let first = cursor.i64()?;
            let last = cursor.i64()?;
            let n = cursor.u32()?;
            let at = u64::from(cursor.u32()?);
            let len = cursor.u32()?;
            if at + u64::from(len) > segment.data_len {
                bail!("a chunk runs past the segment's data");
            }
            chunks.push((first, last, n, segment.data() + at, len));
        }
        out.push(Entry { labels, chunks });
    }
    Ok(out)
}

/// One series' chunk bytes in a segment are contiguous: read them at once.
fn read_chunks(file: &File, chunks: &[(i64, i64, u32, u64, u32)]) -> Result<Vec<Chunk>> {
    let (Some(first), Some(last)) = (chunks.first(), chunks.last()) else {
        return Ok(Vec::new());
    };
    let start = first.3;
    let end = last.3 + u64::from(last.4);
    let bytes = read_at(file, start, end.saturating_sub(start))?;
    chunks
        .iter()
        .map(|&(first_t, last_t, count, at, len)| {
            let from = (at - start) as usize;
            let slice = bytes
                .get(from..from + len as usize)
                .context("chunk outside the bytes read")?;
            Ok(Chunk::from_parts(first_t, last_t, count, slice.into()))
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn read_segment(
    file: &File,
    segment: &Segment,
    name: &dyn Fn(&str) -> bool,
    wanted: &dyn Fn(&Labels) -> bool,
    from: i64,
    to: i64,
    samples: bool,
    out: &mut HashMap<String, (Labels, Vec<Sample>)>,
) -> Result<()> {
    for (metric, offset, len) in read_names(file, segment)? {
        if !name(&metric) {
            continue;
        }
        for entry in read_entries(file, segment, &metric, offset, len)? {
            let overlapping: Vec<_> = entry
                .chunks
                .iter()
                .copied()
                .filter(|c| c.1 >= from && c.0 <= to)
                .collect();
            if overlapping.is_empty() || !wanted(&entry.labels) {
                continue;
            }
            let key = crate::store::canonical_key(&entry.labels);
            let slot = out.entry(key).or_insert_with(|| (entry.labels, Vec::new()));
            if !samples {
                continue;
            }
            let mut decoded = Vec::new();
            for chunk in read_chunks(file, &overlapping)? {
                decoded.clear();
                chunk::decode_into(&chunk, &mut decoded)?;
                slot.1
                    .extend(decoded.iter().filter(|s| s.t >= from && s.t <= to));
            }
        }
    }
    Ok(())
}

/// The day merged into one segment, downsampled when `resolution` is set.
/// Streams one metric name at a time, so it never holds more than that.
fn rewrite(day: &Day, resolution: i64) -> Result<Day> {
    let tmp = day.path.with_extension("klm.tmp");
    let mut names: BTreeMap<String, Vec<(usize, u64, u64)>> = BTreeMap::new();
    for (i, segment) in day.segments.iter().enumerate() {
        for (name, offset, len) in read_names(&day.file, segment)? {
            names.entry(name).or_default().push((i, offset, len));
        }
    }

    let mut out = File::create(&tmp)?;
    out.write_all(&[0u8; HEADER as usize])?;
    let mut builder = Builder::new();
    for (name, places) in &names {
        let mut series: BTreeMap<String, Vec<Chunk>> = BTreeMap::new();
        for &(i, offset, len) in places {
            let segment = &day.segments[i];
            for entry in read_entries(&day.file, segment, name, offset, len)? {
                let chunks = read_chunks(&day.file, &entry.chunks)?;
                series
                    .entry(rest_key(&entry.labels))
                    .or_default()
                    .extend(chunks);
            }
        }
        let mut merged: Vec<(String, Vec<Chunk>)> = Vec::new();
        for (key, chunks) in series {
            merged.push((key, repack(&chunks, resolution)?));
        }
        let mut data = Vec::new();
        builder.add(name, &merged, &mut data)?;
        out.write_all(&data)?;
    }
    let (header, tail) = builder.finish(resolution)?;
    out.write_all(&tail)?;
    out.write_all_at(&header, 0)?;
    out.sync_all()?;
    drop(out);
    std::fs::rename(&tmp, &day.path)?;
    load_day(day.day, day.path.clone())
}

/// A series' chunks for the day packed into one: each hour's chunk would
/// otherwise cost an entry, and a fresh start of the encoding, for good.
/// A sample written twice -- an hour replayed by a crash between a flush
/// and the snapshot after it -- is kept once. With a `resolution`, only
/// the last sample of every bucket that wide is kept.
fn repack(chunks: &[Chunk], resolution: i64) -> Result<Vec<Chunk>> {
    let mut samples: Vec<Sample> = Vec::new();
    for chunk in chunks {
        chunk::decode_into(chunk, &mut samples)?;
    }
    samples.sort_by_key(|s| s.t);
    samples.dedup_by_key(|s| s.t);
    if resolution > 0 {
        let mut kept: Vec<Sample> = Vec::new();
        for sample in samples {
            match kept.last_mut() {
                Some(last) if last.t.div_euclid(resolution) == sample.t.div_euclid(resolution) => {
                    *last = sample;
                }
                _ => kept.push(sample),
            }
        }
        samples = kept;
    }
    Ok(if samples.is_empty() {
        Vec::new()
    } else {
        vec![chunk::encode(&samples)]
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::promql::{eval, parser};
    use crate::store::{Store, labels};

    const T0: i64 = 20_000 * DAY;
    const STEP: i64 = 30;

    fn dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("kubeloupe-disk-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn config(dir: &std::path::Path) -> Config {
        Config {
            dir: dir.to_path_buf(),
            retention: 30 * DAY,
            max_bytes: 1 << 30,
            downsample_after: None,
            resolution: 300,
        }
    }

    /// Feeds both stores the same `days` of samples, flushing the disk one
    /// every hour and maintaining it every day, as the collector does.
    fn feed(memory: &mut Store, disk: &mut Store, days: i64) {
        let mut t = T0;
        while t <= T0 + days * DAY {
            for (pod, rate) in [("a", 3.0), ("b", 0.5)] {
                let n = ((t - T0) / STEP) as f64;
                for store in [&mut *memory, &mut *disk] {
                    store.append(
                        labels("c_total", &[("pod", pod)]),
                        t,
                        n * rate * STEP as f64,
                    );
                    store.append(labels("g", &[("pod", pod)]), t, (n % 17.0) * rate);
                }
            }
            // As the collector does: the first sample of the hour is taken,
            // then the hour before it flushed.
            if t > T0 && t % 3_600 == 0 {
                disk.flush().unwrap();
                if t % DAY == 0 {
                    disk.disk.as_mut().unwrap().maintain(t);
                }
            }
            t += STEP;
        }
    }

    fn answers(store: &Store, at: i64) -> Vec<String> {
        let mut out = Vec::new();
        for query in [
            "g",
            r#"g{pod="a"}"#,
            "sum(rate(c_total[5m]))",
            "increase(c_total[1h])",
            "sum by (pod) (increase(c_total[24h]))",
            r#"{__name__=~"g|h", pod="b"}"#,
        ] {
            let expr = parser::parse(query).unwrap();
            let mut got: Vec<String> = match eval::eval(store, &expr, at) {
                eval::Value::Vector(v) => v
                    .iter()
                    .map(|(l, v)| format!("{query} {l:?} {v}"))
                    .collect(),
                eval::Value::Scalar(s) => vec![format!("{query} {s}")],
            };
            got.sort();
            out.extend(got);
            let range = eval::query_range(store, &expr, at - 6 * 3_600, at, 600);
            let mut lines: Vec<String> = range
                .iter()
                .map(|s| format!("{query} {:?} {:?}", s.labels, s.values))
                .collect();
            lines.sort();
            out.extend(lines);
        }
        out
    }

    #[test]
    fn history_on_disk_answers_exactly_as_memory_would() {
        let dir = dir("same");
        let mut memory = Store::new(40 * DAY);
        let mut disk = Store::new(40 * DAY);
        disk.disk = Some(Disk::open(config(&dir)).unwrap());
        feed(&mut memory, &mut disk, 3);

        assert!(disk.series_count() < memory.series_count() || disk.sample_count() < 1_000);
        // Finished days are merged into one segment each; today has its hours.
        let tier = disk.disk.as_ref().unwrap();
        assert_eq!(tier.days.len(), 3);
        assert!(tier.days[..2].iter().all(|d| d.segments.len() == 1));
        // Each file starts in its own day: midnight's hour, flushed with the
        // new day's first sample, stays with the day it was collected in.
        for d in &tier.days {
            assert_eq!(d.min_t().div_euclid(DAY), d.day, "day {}", d.day - T0 / DAY);
        }
        // And a merged day is one chunk a series, not one an hour.
        let d = &tier.days[0];
        for (name, offset, len) in read_names(&d.file, &d.segments[0]).unwrap() {
            for entry in read_entries(&d.file, &d.segments[0], &name, offset, len).unwrap() {
                assert_eq!(entry.chunks.len(), 1, "{name} {:?}", entry.labels);
            }
        }

        for at in [
            T0 + 3_600,
            T0 + DAY + 1_234,
            T0 + 2 * DAY + 7_200,
            T0 + 3 * DAY - STEP,
        ] {
            assert_eq!(answers(&disk, at), answers(&memory, at), "at {at}");
        }

        // And after a restart, from the files alone.
        let end = T0 + 3 * DAY - STEP;
        disk.flush().unwrap();
        let mut reopened = Store::new(40 * DAY);
        reopened.disk = Some(Disk::open(config(&dir)).unwrap());
        assert_eq!(answers(&reopened, end), answers(&memory, end));

        // The discovery endpoints see what is on disk too.
        let sets = reopened.label_sets(&[], T0, end);
        assert_eq!(sets.len(), 4);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_days_keep_one_point_per_bucket_and_counters_stay_exact() {
        let dir = dir("downsample");
        let mut memory = Store::new(40 * DAY);
        let mut disk = Store::new(40 * DAY);
        let mut cfg = config(&dir);
        cfg.downsample_after = Some(2 * DAY);
        disk.disk = Some(Disk::open(cfg).unwrap());
        feed(&mut memory, &mut disk, 3);
        // A day's file ends with the next midnight's sample, so the hour
        // after midnight's maintenance is the one that sees it past.
        disk.disk.as_mut().unwrap().maintain(T0 + 3 * DAY + 3_600);

        let tier = disk.disk.as_ref().unwrap();
        assert_eq!(tier.days[0].segments[0].resolution, 300);
        assert_eq!(
            tier.days[1].segments[0].resolution, 0,
            "not yet two days old"
        );
        assert!(
            // Only 2x: these series are so regular that a raw day packed
            // into one chunk a series is already tiny.
            tier.days[0].size() * 2 < tier.days[1].size(),
            "downsampled should be smaller"
        );
        assert_eq!(tier.resolution_since(T0), 300);
        assert_eq!(tier.resolution_since(T0 + DAY + 1), 0);

        // A day's increase is exact on bucket edges, downsampled or not.
        let expr = parser::parse(r#"increase(c_total{pod="a"}[12h])"#).unwrap();
        let at = T0 + 18 * 3_600;
        let want = match eval::eval(&memory, &expr, at) {
            eval::Value::Vector(v) => v[0].1,
            _ => unreachable!(),
        };
        match eval::eval(&disk, &expr, at) {
            eval::Value::Vector(v) => assert_eq!(v[0].1, want),
            _ => unreachable!(),
        }

        // A one-minute rate charted at a 30 s step still draws something
        // over downsampled history: the window widens to two points.
        let expr = parser::parse(r#"rate(c_total{pod="a"}[1m])"#).unwrap();
        let range = eval::query_range(&disk, &expr, T0 + 3_600, T0 + 7_200, 30);
        assert_eq!(range.len(), 1);
        assert!(range[0].values.len() > 100);
        assert!(range[0].values.iter().all(|(_, v)| (v - 3.0).abs() < 1e-9));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn retention_and_the_size_cap_delete_whole_days() {
        let dir = dir("retention");
        let mut memory = Store::new(40 * DAY);
        let mut disk = Store::new(40 * DAY);
        let mut cfg = config(&dir);
        cfg.retention = 2 * DAY;
        disk.disk = Some(Disk::open(cfg).unwrap());
        feed(&mut memory, &mut disk, 4);
        let tier = disk.disk.as_mut().unwrap();
        // As in the downsampling test: the hour after midnight's.
        tier.maintain(T0 + 4 * DAY + 3_600);
        let days: Vec<i64> = tier.days.iter().map(|d| d.day - T0 / DAY).collect();
        assert_eq!(days, [2, 3], "only two days kept");

        tier.config.max_bytes = 1;
        tier.maintain(T0 + 4 * DAY - 1);
        let days: Vec<i64> = tier.days.iter().map(|d| d.day - T0 / DAY).collect();
        assert_eq!(days, [3], "today is never dropped for size");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_torn_segment_is_cut_off_and_the_rest_still_reads() {
        let dir = dir("torn");
        let mut memory = Store::new(40 * DAY);
        let mut disk = Store::new(40 * DAY);
        disk.disk = Some(Disk::open(config(&dir)).unwrap());
        feed(&mut memory, &mut disk, 1);
        let at = T0 + DAY - 3_600;
        let before = answers(&disk, at);
        let path = disk.disk.as_ref().unwrap().days[0].path.clone();
        let len = std::fs::metadata(&path).unwrap().len();
        // Half a segment, as a crash mid-write would leave.
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"KLM1\0\0\0\0garbage that stops short")
            .unwrap();
        drop(file);
        drop(disk);

        let mut reopened = Store::new(40 * DAY);
        reopened.disk = Some(Disk::open(config(&dir)).unwrap());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), len);
        assert_eq!(answers(&reopened, at), before);

        // A corrupt entry table is an error for that segment, not a panic.
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        file.write_all_at(&[0xff; 64], len - 64).unwrap();
        drop(file);
        let mut broken = Store::new(40 * DAY);
        broken.disk = Some(Disk::open(config(&dir)).unwrap());
        let _ = answers(&broken, at);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
