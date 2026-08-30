//! Source discovery and boundary-complete Record reading.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result, bail};

pub(crate) const READ_BUFFER_BYTES: usize = 64 * 1024;

pub(crate) fn discover_jsonl_files(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut files = BTreeSet::new();
    for path in paths {
        collect_jsonl_files(path, &mut files)?;
    }
    Ok(files.into_iter().collect())
}

fn collect_jsonl_files(path: &Path, files: &mut BTreeSet<PathBuf>) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    if metadata.is_file() {
        if path
            .extension()
            .is_some_and(|extension| extension == "jsonl")
        {
            files.insert(path.canonicalize()?);
        }
        return Ok(());
    }
    if !metadata.is_dir() {
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

pub(crate) fn fingerprint_prefix_into(
    file: &mut File,
    length: u64,
    destination: &mut blake3::Hasher,
) -> Result<String> {
    let mut validation = prefix_hasher(length);
    file.seek(SeekFrom::Start(0))?;
    hash_exact(file, length, &mut validation, Some(destination))?;
    Ok(validation.finalize().to_hex().to_string())
}

fn hash_exact(
    file: &mut File,
    mut remaining: u64,
    hasher: &mut blake3::Hasher,
    mut destination: Option<&mut blake3::Hasher>,
) -> Result<()> {
    let mut buffer = vec![0_u8; READ_BUFFER_BYTES].into_boxed_slice();
    while remaining > 0 {
        let requested = usize::try_from(remaining.min(buffer.len() as u64))?;
        let count = file.read(&mut buffer[..requested])?;
        if count == 0 {
            bail!("file ended while fingerprinting an indexed prefix");
        }
        hasher.update(&buffer[..count]);
        if let Some(destination) = destination.as_deref_mut() {
            destination.update(&buffer[..count]);
        }
        remaining -= u64::try_from(count)?;
    }
    Ok(())
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
