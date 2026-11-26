//! Source discovery and boundary-complete Record reading.

use std::collections::hash_map::Entry;
use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Cursor, Read, Seek, SeekFrom, Take, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result, bail};

use crate::adapters::runtime::antigravity;

pub(crate) const READ_BUFFER_BYTES: usize = 64 * 1024;
const DECOMPRESS_BUFFER_BYTES: usize = 256 * 1024;
const PREFIX_VERIFY_BUFFER_BYTES: usize = 256 * 1024;
/// Upper bound for holding a decompressed `.jsonl.zst` archive in the reusable
/// run-scoped buffer before spilling to an anonymous temporary file on disk.
const MAX_IN_MEMORY_DECOMPRESSED_BYTES: usize = 32 * 1024 * 1024;

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

    let canonical_dir = path
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", path.display()))?;
    collect_canonical_dir(&canonical_dir, files)
}

fn collect_canonical_dir(dir: &Path, files: &mut BTreeSet<PathBuf>) -> Result<()> {
    // Antigravity conversation directories hold thousands of per-step artifact
    // subdirectories under `.system_generated/steps/`. Only the two consolidated
    // transcripts in `.system_generated/logs/` are valid index inputs.
    if antigravity::conversation_dir(dir) {
        let logs = dir.join(".system_generated").join("logs");
        if fs::symlink_metadata(&logs).is_ok_and(|meta| meta.is_dir()) {
            collect_antigravity_logs(&logs, files);
        }
        return Ok(());
    }

    // Also short-circuit when the caller passes `.../.system_generated/logs`
    // directly so chunk copies beside the consolidated transcripts are ignored.
    let short_transcript = dir.join("transcript.jsonl");
    if antigravity::transcript(&short_transcript).is_some() {
        collect_antigravity_logs(dir, files);
        return Ok(());
    }

    for entry in
        fs::read_dir(dir).with_context(|| format!("failed to read directory {}", dir.display()))?
    {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        let child = dir.join(entry.file_name());
        if file_type.is_dir() {
            collect_canonical_dir(&child, files)?;
        } else if file_type.is_file() && is_trace_file(&child) {
            // `dir` is canonical and no symlink was traversed, so `child` is
            // already canonical without an extra `realpath` syscall.
            files.insert(child);
        }
    }
    Ok(())
}

fn collect_antigravity_logs(logs_dir: &Path, files: &mut BTreeSet<PathBuf>) {
    for name in ["transcript_full.jsonl", "transcript.jsonl"] {
        let candidate = logs_dir.join(name);
        if let Ok(meta) = fs::symlink_metadata(&candidate)
            && !meta.file_type().is_symlink()
            && meta.is_file()
        {
            files.insert(candidate);
            return;
        }
    }
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

/// Seekable decoded view of one Source during indexing.
///
/// Plain `.jsonl` files stay file-backed with zero copying. Compressed
/// `.jsonl.zst` archives decode into the caller's reusable buffer up to
/// [`MAX_IN_MEMORY_DECOMPRESSED_BYTES`] to avoid temporary-file write/read I/O,
/// spilling transparently to an anonymous temporary file for larger archives so
/// memory remains strictly bounded.
pub(crate) enum IndexingSource<'a> {
    File { file: File, len: u64 },
    Memory(Cursor<&'a [u8]>),
}

impl IndexingSource<'_> {
    pub(crate) fn len(&self) -> u64 {
        match self {
            Self::File { len, .. } => *len,
            Self::Memory(cursor) => cursor.get_ref().len() as u64,
        }
    }

    pub(crate) fn bounded_reader(&mut self, limit: u64) -> Result<BoundedSourceReader<'_>> {
        self.bounded_reader_from(0, limit)
    }

    pub(crate) fn bounded_reader_from(
        &mut self,
        start: u64,
        limit: u64,
    ) -> Result<BoundedSourceReader<'_>> {
        let span = limit.saturating_sub(start);
        match self {
            Self::File { file, .. } => {
                file.seek(SeekFrom::Start(start))?;
                Ok(BoundedSourceReader::File(BufReader::with_capacity(
                    READ_BUFFER_BYTES,
                    file.take(span),
                )))
            }
            Self::Memory(cursor) => {
                let bytes = cursor.get_ref();
                let start_idx = usize::try_from(start)?.min(bytes.len());
                let end_idx = usize::try_from(limit)?.min(bytes.len()).max(start_idx);
                Ok(BoundedSourceReader::Memory(Cursor::new(
                    &bytes[start_idx..end_idx],
                )))
            }
        }
    }

    pub(crate) fn complete_prefix_length(&mut self) -> Result<u64> {
        match self {
            Self::Memory(cursor) => Ok(cursor
                .get_ref()
                .iter()
                .rposition(|byte| *byte == b'\n')
                .map_or(0, |index| (index + 1) as u64)),
            Self::File { file, len } => complete_prefix_length_file(file, *len),
        }
    }

    pub(crate) fn fingerprint_prefix(&mut self, length: u64) -> Result<String> {
        match self {
            Self::Memory(cursor) => {
                let want = usize::try_from(length)?;
                let bytes = cursor.get_ref();
                if want > bytes.len() {
                    bail!("file ended while fingerprinting an indexed prefix");
                }
                let mut hash = prefix_hasher(length);
                hash.update(&bytes[..want]);
                Ok(hash.finalize().to_hex().to_string())
            }
            Self::File { file, .. } => fingerprint_prefix_file(file, length),
        }
    }

    /// Verifies that `[0..old_len]` still matches `expected_fingerprint` while
    /// seeding a whole-prefix `BLAKE3` hasher for `new_len` in one sequential
    /// chunked read pass.
    ///
    /// Returns `Ok(Some(full_hasher))` positioned logically at byte `old_len`
    /// when the prefix is intact, or `Ok(None)` if the prefix was rewritten.
    pub(crate) fn verify_and_seed_prefix(
        &mut self,
        old_len: u64,
        expected_fingerprint: &str,
        new_len: u64,
    ) -> Result<Option<blake3::Hasher>> {
        match self {
            Self::Memory(cursor) => {
                let want = usize::try_from(old_len)?;
                let bytes = cursor.get_ref();
                if want > bytes.len() {
                    return Ok(None);
                }
                let prefix = &bytes[..want];
                let mut verify = prefix_hasher(old_len);
                verify.update(prefix);
                if verify.finalize().to_hex().as_str() != expected_fingerprint {
                    return Ok(None);
                }
                let mut full = prefix_hasher(new_len);
                full.update(prefix);
                Ok(Some(full))
            }
            Self::File { file, .. } => {
                verify_and_seed_prefix_file(file, old_len, expected_fingerprint, new_len)
            }
        }
    }
}

impl Read for IndexingSource<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::File { file, .. } => file.read(buf),
            Self::Memory(cursor) => cursor.read(buf),
        }
    }
}

impl Seek for IndexingSource<'_> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        match self {
            Self::File { file, .. } => file.seek(pos),
            Self::Memory(cursor) => cursor.seek(pos),
        }
    }
}

/// Zero-copy `BufRead` view over an [`IndexingSource`] prefix.
pub(crate) enum BoundedSourceReader<'a> {
    File(BufReader<Take<&'a mut File>>),
    Memory(Cursor<&'a [u8]>),
}

impl Read for BoundedSourceReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::File(reader) => reader.read(buf),
            Self::Memory(cursor) => cursor.read(buf),
        }
    }
}

impl BufRead for BoundedSourceReader<'_> {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        match self {
            Self::File(reader) => reader.fill_buf(),
            Self::Memory(cursor) => cursor.fill_buf(),
        }
    }

    fn consume(&mut self, amt: usize) {
        match self {
            Self::File(reader) => reader.consume(amt),
            Self::Memory(cursor) => cursor.consume(amt),
        }
    }
}

/// Opens a Source for indexing, decoding compressed archives into `scratch`
/// when they fit within [`MAX_IN_MEMORY_DECOMPRESSED_BYTES`] and spilling
/// larger archives to an anonymous temporary file.
pub(crate) fn open_for_indexing<'a>(
    path: &Path,
    scratch: &'a mut Vec<u8>,
) -> Result<IndexingSource<'a>> {
    open_for_indexing_with_limit(path, scratch, MAX_IN_MEMORY_DECOMPRESSED_BYTES)
}

fn open_for_indexing_with_limit<'a>(
    path: &Path,
    scratch: &'a mut Vec<u8>,
    max_in_memory: usize,
) -> Result<IndexingSource<'a>> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    if !is_compressed(path) {
        let len = file.metadata()?.len();
        return Ok(IndexingSource::File { file, len });
    }
    scratch.clear();
    let mut archive = zstd::stream::read::Decoder::new(file)?;
    let mut chunk = vec![0_u8; DECOMPRESS_BUFFER_BYTES].into_boxed_slice();
    let mut spilled: Option<(File, u64)> = None;
    loop {
        let count = archive
            .read(&mut chunk)
            .with_context(|| format!("failed to decompress {}", path.display()))?;
        if count == 0 {
            break;
        }
        if let Some((decoded, written)) = spilled.as_mut() {
            decoded.write_all(&chunk[..count]).with_context(|| {
                format!(
                    "failed to write decompression snapshot for {}",
                    path.display()
                )
            })?;
            *written += u64::try_from(count)?;
        } else if scratch.len().saturating_add(count) <= max_in_memory {
            scratch.extend_from_slice(&chunk[..count]);
        } else {
            let mut decoded =
                tempfile::tempfile().context("failed to create a decompression snapshot")?;
            decoded.write_all(scratch).with_context(|| {
                format!(
                    "failed to write decompression snapshot for {}",
                    path.display()
                )
            })?;
            decoded.write_all(&chunk[..count]).with_context(|| {
                format!(
                    "failed to write decompression snapshot for {}",
                    path.display()
                )
            })?;
            let written = u64::try_from(scratch.len())? + u64::try_from(count)?;
            scratch.clear();
            spilled = Some((decoded, written));
        }
    }
    if let Some((mut decoded, len)) = spilled {
        decoded.rewind()?;
        Ok(IndexingSource::File { file: decoded, len })
    } else {
        Ok(IndexingSource::Memory(Cursor::new(scratch.as_slice())))
    }
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
    Complete(Option<Vec<u8>>),
    Incomplete,
    End,
}

pub(crate) fn read_bounded_record(
    reader: &mut impl BufRead,
    max_record_bytes: usize,
) -> Result<RecordRead> {
    let mut scratch = Vec::new();
    match read_bounded_record_into(reader, max_record_bytes, None, false, &mut scratch)? {
        RecordReadInto::Complete { oversized, .. } => {
            Ok(RecordRead::Complete((!oversized).then_some(scratch)))
        }
        RecordReadInto::Incomplete => Ok(RecordRead::Incomplete),
        RecordReadInto::End => Ok(RecordRead::End),
    }
}

#[derive(Debug)]
pub(crate) enum RecordReadInto {
    Complete {
        oversized: bool,
        byte_length: u64,
        consumed_bytes: u64,
        raw_hash: String,
    },
    Incomplete,
    End,
}

/// Tracks the new whole-prefix `BLAKE3` digest while optionally verifying an
/// earlier indexed byte prefix during the same sequential read pass.
pub(crate) struct PrefixVerify<'a> {
    pub(crate) hasher: &'a mut blake3::Hasher,
    pub(crate) remaining: u64,
}

/// Reads one newline-terminated Record into a caller-reused byte buffer.
///
/// Skipping `compute_record_hash` avoids hashing already-persisted prefix lines
/// during an incremental append where only the projection and whole-prefix
/// fingerprint are needed.
pub(crate) fn read_bounded_record_into(
    reader: &mut impl BufRead,
    max_record_bytes: usize,
    prefix_hasher: Option<&mut blake3::Hasher>,
    compute_record_hash: bool,
    buffer: &mut Vec<u8>,
) -> Result<RecordReadInto> {
    read_bounded_record_inner(
        reader,
        max_record_bytes,
        prefix_hasher,
        None,
        compute_record_hash,
        buffer,
    )
}

/// Reads one newline-terminated Record while updating both the new whole-prefix
/// digest and an optional verification digest over the previously indexed prefix.
pub(crate) fn read_bounded_record_with_verify(
    reader: &mut impl BufRead,
    max_record_bytes: usize,
    prefix_hasher: Option<&mut blake3::Hasher>,
    verify: Option<&mut PrefixVerify<'_>>,
    compute_record_hash: bool,
    buffer: &mut Vec<u8>,
) -> Result<RecordReadInto> {
    read_bounded_record_inner(
        reader,
        max_record_bytes,
        prefix_hasher,
        verify,
        compute_record_hash,
        buffer,
    )
}

fn read_bounded_record_inner(
    reader: &mut impl BufRead,
    max_record_bytes: usize,
    mut prefix_hasher: Option<&mut blake3::Hasher>,
    mut verify: Option<&mut PrefixVerify<'_>>,
    compute_record_hash: bool,
    buffer: &mut Vec<u8>,
) -> Result<RecordReadInto> {
    buffer.clear();
    let mut oversized = false;
    let mut byte_length = 0_u64;
    let mut consumed_bytes = 0_u64;
    let mut hasher = compute_record_hash.then(blake3::Hasher::new);

    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return if consumed_bytes == 0 {
                Ok(RecordReadInto::End)
            } else {
                Ok(RecordReadInto::Incomplete)
            };
        }

        let newline = available.iter().position(|byte| *byte == b'\n');
        let data_len = newline.unwrap_or(available.len());
        let data = &available[..data_len];
        if let Some(hasher) = hasher.as_mut() {
            hasher.update(data);
        }
        byte_length += u64::try_from(data.len())?;

        if !oversized {
            if buffer.len().saturating_add(data.len()) <= max_record_bytes {
                buffer.extend_from_slice(data);
            } else {
                buffer.clear();
                oversized = true;
            }
        }

        let consume = data_len + usize::from(newline.is_some());
        let consumed_slice = &available[..consume];
        if let Some(hasher) = prefix_hasher.as_deref_mut() {
            hasher.update(consumed_slice);
        }
        if let Some(verify) = verify.as_deref_mut()
            && verify.remaining > 0
        {
            let take = usize::try_from(verify.remaining.min(consumed_slice.len() as u64))?;
            verify.hasher.update(&consumed_slice[..take]);
            verify.remaining -= u64::try_from(take)?;
        }
        reader.consume(consume);
        consumed_bytes += u64::try_from(consume)?;

        if newline.is_some() {
            let raw_hash = hasher
                .map(|hasher| hasher.finalize().to_hex().to_string())
                .unwrap_or_default();
            return Ok(RecordReadInto::Complete {
                oversized,
                byte_length,
                consumed_bytes,
                raw_hash,
            });
        }
    }
}

pub(crate) fn prefix_hasher(length: u64) -> blake3::Hasher {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&length.to_le_bytes());
    hasher
}

fn fingerprint_prefix_file(file: &mut File, length: u64) -> Result<String> {
    let mut hash = prefix_hasher(length);
    file.seek(SeekFrom::Start(0))?;
    let mut remaining = length;
    let mut buffer = vec![0_u8; PREFIX_VERIFY_BUFFER_BYTES].into_boxed_slice();
    while remaining > 0 {
        let want = usize::try_from(remaining.min(buffer.len() as u64))?;
        let count = file.read(&mut buffer[..want])?;
        if count == 0 {
            bail!("file ended while fingerprinting an indexed prefix");
        }
        hash.update(&buffer[..count]);
        remaining -= u64::try_from(count)?;
    }
    Ok(hash.finalize().to_hex().to_string())
}

fn verify_and_seed_prefix_file(
    file: &mut File,
    old_len: u64,
    expected_fingerprint: &str,
    new_len: u64,
) -> Result<Option<blake3::Hasher>> {
    let mut verify = prefix_hasher(old_len);
    let mut full = prefix_hasher(new_len);
    file.seek(SeekFrom::Start(0))?;
    let mut remaining = old_len;
    let mut buffer = vec![0_u8; PREFIX_VERIFY_BUFFER_BYTES].into_boxed_slice();
    while remaining > 0 {
        let want = usize::try_from(remaining.min(buffer.len() as u64))?;
        let count = file.read(&mut buffer[..want])?;
        if count == 0 {
            return Ok(None);
        }
        let chunk = &buffer[..count];
        verify.update(chunk);
        full.update(chunk);
        remaining -= u64::try_from(count)?;
    }
    if verify.finalize().to_hex().as_str() != expected_fingerprint {
        return Ok(None);
    }
    Ok(Some(full))
}

fn complete_prefix_length_file(file: &mut File, file_size: u64) -> Result<u64> {
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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write as _;

    use tempfile::tempdir;

    use super::{IndexingSource, discover_jsonl_files, open_for_indexing_with_limit};

    #[test]
    fn antigravity_discovery_skips_step_subdirectories_and_prefers_full_transcript() {
        let root = tempdir().expect("tempdir");
        let brain = root.path().join("brain");
        let conv = brain.join("955c79ca-6f59-46b9-8a47-c8ab27227b8a");
        let logs = conv.join(".system_generated").join("logs");
        let step_dir = conv.join(".system_generated").join("steps").join("42");
        fs::create_dir_all(&logs).expect("create logs dir");
        fs::create_dir_all(&step_dir).expect("create steps dir");

        let short = logs.join("transcript.jsonl");
        let full = logs.join("transcript_full.jsonl");
        let chunk = logs.join("transcript_chunk_001.jsonl");
        let stray = step_dir.join("output.jsonl");
        fs::write(&short, "{}\n").expect("write short");
        fs::write(&full, "{}\n").expect("write full");
        fs::write(&chunk, "{}\n").expect("write chunk");
        fs::write(&stray, "{}\n").expect("write stray");

        let discovered = discover_jsonl_files(&[brain]).expect("discover");
        assert_eq!(discovered, vec![full.canonicalize().expect("canonical")]);
    }

    #[test]
    fn compressed_source_decodes_in_memory_and_spills_past_limit() {
        let root = tempdir().expect("tempdir");
        let archive_path = root.path().join("sample.jsonl.zst");
        let payload = b"{\"line\":1}\n{\"line\":2}\ntrailing";
        let compressed = zstd::bulk::compress(payload, 3).expect("compress");
        fs::File::create(&archive_path)
            .expect("create archive")
            .write_all(&compressed)
            .expect("write archive");

        let mut scratch = Vec::new();
        let mut mem_source =
            open_for_indexing_with_limit(&archive_path, &mut scratch, 1024).expect("in-memory");
        assert!(matches!(mem_source, IndexingSource::Memory(_)));
        assert_eq!(mem_source.len(), payload.len() as u64);
        assert_eq!(mem_source.complete_prefix_length().expect("prefix len"), 22);
        let mem_fp = mem_source.fingerprint_prefix(22).expect("mem fingerprint");

        let mut spilled_source =
            open_for_indexing_with_limit(&archive_path, &mut scratch, 8).expect("spilled");
        assert!(matches!(spilled_source, IndexingSource::File { .. }));
        assert_eq!(spilled_source.len(), payload.len() as u64);
        assert_eq!(
            spilled_source.complete_prefix_length().expect("prefix len"),
            22
        );
        let file_fp = spilled_source
            .fingerprint_prefix(22)
            .expect("file fingerprint");
        assert_eq!(mem_fp, file_fp);
    }

    #[test]
    fn verify_and_seed_prefix_matches_full_fingerprint_and_rejects_mutation() {
        use std::io::Read as _;

        let root = tempdir().expect("tempdir");
        let trace = root.path().join("tail.jsonl");
        let prefix = b"{\"line\":1}\n{\"line\":2}\n";
        let tail = b"{\"line\":3}\n{\"line\":4}\n";
        let mut combined = Vec::new();
        combined.extend_from_slice(prefix);
        combined.extend_from_slice(tail);
        fs::write(&trace, &combined).expect("write trace");

        let old_len = prefix.len() as u64;
        let new_len = combined.len() as u64;

        let mut scratch = Vec::new();
        let mut file_src =
            open_for_indexing_with_limit(&trace, &mut scratch, 1024).expect("open file");
        let expected_old = file_src.fingerprint_prefix(old_len).expect("old fp");
        let expected_new = file_src.fingerprint_prefix(new_len).expect("new fp");

        let mut seeded = file_src
            .verify_and_seed_prefix(old_len, &expected_old, new_len)
            .expect("verify file")
            .expect("prefix should verify");
        let mut tail_reader = file_src
            .bounded_reader_from(old_len, new_len)
            .expect("tail reader");
        let mut read_tail = Vec::new();
        tail_reader.read_to_end(&mut read_tail).expect("read tail");
        assert_eq!(read_tail, tail);
        seeded.update(&read_tail);
        assert_eq!(seeded.finalize().to_hex().to_string(), expected_new);

        assert!(
            file_src
                .verify_and_seed_prefix(old_len, "deadbeef", new_len)
                .expect("mismatch check")
                .is_none()
        );

        let mut mem_src = IndexingSource::Memory(std::io::Cursor::new(&combined));
        let mut mem_seeded = mem_src
            .verify_and_seed_prefix(old_len, &expected_old, new_len)
            .expect("verify memory")
            .expect("memory prefix should verify");
        let mut mem_tail = Vec::new();
        mem_src
            .bounded_reader_from(old_len, new_len)
            .expect("mem tail reader")
            .read_to_end(&mut mem_tail)
            .expect("read mem tail");
        assert_eq!(mem_tail, tail);
        mem_seeded.update(&mem_tail);
        assert_eq!(mem_seeded.finalize().to_hex().to_string(), expected_new);
    }
}
