//! Source discovery and boundary-complete Record reading.

use std::collections::hash_map::Entry;
use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result, bail};

use crate::adapters::runtime::antigravity;

pub(crate) const READ_BUFFER_BYTES: usize = 64 * 1024;

pub(crate) fn discover_jsonl_files(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut files = BTreeSet::new();
    for path in paths {
        collect_jsonl_files(path, &mut files)?;
    }
    let shadowed: Vec<_> = files
        .iter()
        .filter(|path| is_compressed(path) && files.contains(&path.with_extension("")))
        .cloned()
        .collect();
    for path in shadowed {
        files.remove(&path);
    }
    prefer_antigravity_transcripts(&mut files);
    Ok(files.into_iter().collect())
}

fn prefer_antigravity_transcripts(files: &mut BTreeSet<PathBuf>) {
    let mut preferred = HashMap::new();
    let mut shadowed = Vec::new();
    for path in files.iter() {
        let Some((id, full)) = antigravity::transcript(path) else {
            continue;
        };
        match preferred.entry(id) {
            Entry::Vacant(entry) => {
                entry.insert((path, full));
            }
            Entry::Occupied(mut entry) if full && !entry.get().1 => {
                shadowed.push(entry.insert((path, full)).0.clone());
            }
            Entry::Occupied(_) => shadowed.push(path.clone()),
        }
    }
    for path in shadowed {
        files.remove(&path);
    }
}

fn collect_jsonl_files(path: &Path, files: &mut BTreeSet<PathBuf>) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    if metadata.is_file() {
        if is_trace_file(path) {
            files.insert(path.canonicalize()?);
        }
        return Ok(());
    }
    if !metadata.is_dir() {
        return Ok(());
    }

    // Antigravity's logs directory also contains chunk copies of its two
    // transcript projections. Only the producer's consolidated files are
    // index inputs.
    let short_transcript = path.join("transcript.jsonl");
    if antigravity::transcript(&short_transcript).is_some() {
        for candidate in [short_transcript, path.join("transcript_full.jsonl")] {
            if candidate.is_file() {
                collect_jsonl_files(&candidate, files)?;
            }
        }
        return Ok(());
    }

    for entry in fs::read_dir(path)
        .with_context(|| format!("failed to read directory {}", path.display()))?
    {
        let entry = entry?;
        collect_jsonl_files(&entry.path(), files)?;
    }
    Ok(())
}

pub(crate) fn is_trace_file(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension == "jsonl")
        || is_compressed(path)
}

pub(crate) fn is_compressed(path: &Path) -> bool {
    path.extension().is_some_and(|extension| extension == "zst")
        && path
            .with_extension("")
            .extension()
            .is_some_and(|extension| extension == "jsonl")
}

pub(crate) fn alternate_encoding(path: &Path) -> PathBuf {
    if is_compressed(path) {
        path.with_extension("")
    } else {
        path.with_extension("jsonl.zst")
    }
}

/// Replay needs a seekable snapshot to find the last complete Record and
/// compare a published prefix. Decode into an anonymous temporary file so
/// disk holds the snapshot even for an archive larger than memory.
pub(crate) fn open_for_indexing(path: &Path) -> Result<File> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    if !is_compressed(path) {
        return Ok(file);
    }
    let mut decoded = tempfile::tempfile().context("failed to create a decompression snapshot")?;
    let mut archive = zstd::stream::read::Decoder::new(file)?;
    std::io::copy(&mut archive, &mut decoded)
        .with_context(|| format!("failed to decompress {}", path.display()))?;
    decoded.rewind()?;
    Ok(decoded)
}

/// Opens a reader at a decoded byte offset. Plain Sources seek directly.
/// Compressed Sources only need to decode as far as the requested Record.
pub(crate) fn read_at(file: File, path: &Path, offset: u64) -> Result<Option<Box<dyn Read>>> {
    if is_compressed(path) {
        let decoded = zstd::stream::read::Decoder::new(file)?;
        let mut archive = BufReader::with_capacity(READ_BUFFER_BYTES, decoded);
        if std::io::copy(&mut archive.by_ref().take(offset), &mut std::io::sink())? != offset
            || archive.fill_buf()?.is_empty()
        {
            return Ok(None);
        }
        Ok(Some(Box::new(archive)))
    } else {
        let mut file = file;
        if offset >= file.metadata()?.len() {
            return Ok(None);
        }
        file.seek(SeekFrom::Start(offset))?;
        Ok(Some(Box::new(file)))
    }
}

#[derive(Debug)]
pub(crate) enum RecordRead {
    Complete(BoundedRecord),
    Incomplete,
    End,
}

#[derive(Debug)]
pub(crate) struct BoundedRecord {
    pub bytes: Option<Vec<u8>>,
    pub byte_length: u64,
    pub consumed_bytes: u64,
    pub raw_hash: String,
}

pub(crate) fn read_bounded_record(
    reader: &mut impl BufRead,
    max_record_bytes: usize,
) -> Result<RecordRead> {
    read_bounded_record_hashed(reader, max_record_bytes, None)
}

pub(crate) fn read_bounded_record_hashed(
    reader: &mut impl BufRead,
    max_record_bytes: usize,
    mut prefix_hasher: Option<&mut blake3::Hasher>,
) -> Result<RecordRead> {
    let mut bytes = Some(Vec::new());
    let mut byte_length = 0_u64;
    let mut consumed_bytes = 0_u64;
    let mut hasher = blake3::Hasher::new();

    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return if consumed_bytes == 0 {
                Ok(RecordRead::End)
            } else {
                Ok(RecordRead::Incomplete)
            };
        }

        let newline = available.iter().position(|byte| *byte == b'\n');
        let data_len = newline.unwrap_or(available.len());
        let data = &available[..data_len];
        hasher.update(data);
        byte_length += u64::try_from(data.len())?;

        if let Some(buffer) = &mut bytes {
            if buffer.len().saturating_add(data.len()) <= max_record_bytes {
                buffer.extend_from_slice(data);
            } else {
                bytes = None;
            }
        }

        let consume = data_len + usize::from(newline.is_some());
        if let Some(hasher) = prefix_hasher.as_deref_mut() {
            hasher.update(&available[..consume]);
        }
        reader.consume(consume);
        consumed_bytes += u64::try_from(consume)?;

        if newline.is_some() {
            return Ok(RecordRead::Complete(BoundedRecord {
                bytes,
                byte_length,
                consumed_bytes,
                raw_hash: hasher.finalize().to_hex().to_string(),
            }));
        }
    }
}

pub(crate) fn prefix_hasher(length: u64) -> blake3::Hasher {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&length.to_le_bytes());
    hasher
}

pub(crate) fn fingerprint_prefix(file: &mut File, length: u64) -> Result<String> {
    let mut hash = prefix_hasher(length);
    file.seek(SeekFrom::Start(0))?;
    let mut prefix = BufReader::with_capacity(READ_BUFFER_BYTES, file.take(length));
    if std::io::copy(&mut prefix, &mut hash)? != length {
        bail!("file ended while fingerprinting an indexed prefix");
    }
    Ok(hash.finalize().to_hex().to_string())
}

pub(crate) fn complete_prefix_length(file: &mut File, file_size: u64) -> Result<u64> {
    let mut end = file_size;
    let mut buffer = vec![0_u8; READ_BUFFER_BYTES].into_boxed_slice();
    while end > 0 {
        let count = usize::try_from(end.min(buffer.len() as u64))?;
        let start = end - u64::try_from(count)?;
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut buffer[..count])?;
        if let Some(index) = buffer[..count].iter().rposition(|byte| *byte == b'\n') {
            return Ok(start + u64::try_from(index)? + 1);
        }
        end = start;
    }
    Ok(0)
}

pub(crate) fn modified_ns(metadata: &fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .and_then(|duration| i64::try_from(duration.as_nanos()).ok())
        .unwrap_or(0)
}
