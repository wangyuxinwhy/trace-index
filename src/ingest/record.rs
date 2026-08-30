//! Explicit raw Record inspection and Asset extraction commands.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use rusqlite::params;
use serde_json::{Value, json};

use crate::interface::output::{
    AssetDescriptor, AssetExtractReport, Record, RecordExportReport, RecordParse, RecordResponse,
    RecordSource, RecordVerification,
};
use crate::storage::db::Store;

const REFERENCE_KEY: &str = "$trace_ref";

/// Returns one Record and a display-safe representation of its source JSON.
///
/// Large scalar values and inline data assets are replaced by reversible
/// Record-and-JSON-Pointer references.
///
/// # Errors
///
/// Returns an error when the Record is unknown, its source cannot be read, or
/// its stored numeric facts are invalid.
pub fn inspect_record(
    store: &Store,
    record_id: i64,
    max_record_bytes: u64,
    max_value_bytes: usize,
    max_output_bytes: usize,
) -> Result<RecordResponse> {
    let record = load_record(store, record_id)?;
    let verification = verify_loaded_record(&record)?;
    let verified = verification.status == "verified";
    let mut externalized_values = 0;
    let mut representation = if !verified {
        external_reference(
            "record",
            record.id,
            "",
            Some(json!({"reason": verification.status})),
        )
    } else if record.byte_length > max_record_bytes {
        externalized_values += 1;
        external_reference(
            "record",
            record.id,
            "",
            Some(json!({
                "reason": "record_read_budget",
                "byte_length": record.byte_length,
                "max_record_bytes": max_record_bytes
            })),
        )
    } else {
        let bytes = read_record_bytes(&record)?;
        if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
            externalize_value(
                &value,
                record.id,
                "",
                max_value_bytes,
                &mut externalized_values,
            )
        } else {
            let text = String::from_utf8_lossy(&bytes);
            externalize_string(
                &text,
                record.id,
                "",
                max_value_bytes,
                &mut externalized_values,
            )
        }
    };

    let mut representation_bytes = serde_json::to_vec(&representation)?.len();
    if representation_bytes > max_output_bytes {
        externalized_values += 1;
        representation = external_reference(
            "record",
            record.id,
            "",
            Some(json!({
                "reason": "representation_output_budget",
                "representation_bytes": representation_bytes,
                "max_output_bytes": max_output_bytes
            })),
        );
        representation_bytes = serde_json::to_vec(&representation)?.len();
    }

    Ok(RecordResponse {
        record,
        raw_verified: verified,
        representation,
        externalized_values,
        representation_bytes,
    })
}

/// Verifies one Record against its original source byte range.
///
/// # Errors
///
/// Returns an error when the Record is unknown or its source cannot be opened
/// or read.
pub fn verify_record(store: &Store, record_id: i64) -> Result<RecordVerification> {
    let record = load_record(store, record_id)?;
    verify_loaded_record(&record)
}

/// Writes one verified, byte-exact Record to a local file.
///
/// # Errors
///
/// Returns an error when verification fails, the Record exceeds the explicit
/// export budget, the destination exists without `force`, or I/O fails.
pub fn export_record(
    store: &Store,
    record_id: i64,
    output: &Path,
    max_bytes: u64,
    force: bool,
) -> Result<RecordExportReport> {
    let record = load_record(store, record_id)?;
    if record.byte_length > max_bytes {
        bail!(
            "record is {} bytes, exceeding --max-bytes={max_bytes}",
            record.byte_length
        );
    }
    let bytes = read_record_bytes(&record)?;
    let actual_hash = blake3::hash(&bytes).to_hex().to_string();
    if actual_hash != record.raw_hash {
        bail!(
            "record verification failed: expected {}, got {actual_hash}",
            record.raw_hash
        );
    }
    write_private_file(output, &bytes, force)?;
    Ok(RecordExportReport {
        record_id: record.id,
        output: output.display().to_string(),
        byte_length: record.byte_length,
        raw_hash: record.raw_hash,
    })
}

/// Decodes one inline asset and writes it to a local file.
///
/// # Errors
///
/// Returns an error when resolution or decoding fails, the decoded payload
/// exceeds the explicit budget, or the destination cannot be written.
pub fn extract_asset(
    store: &Store,
    reference: &str,
    output: &Path,
    max_record_bytes: u64,
    max_asset_bytes: usize,
    force: bool,
) -> Result<AssetExtractReport> {
    let (asset, encoded) = load_asset(store, reference, max_record_bytes)?;
    let estimated_bytes = encoded.len().saturating_add(3) / 4 * 3;
    if estimated_bytes > max_asset_bytes {
        bail!(
            "decoded asset is approximately {estimated_bytes} bytes, exceeding \
             --max-bytes={max_asset_bytes}"
        );
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .context("failed to decode inline base64 asset")?;
    if bytes.len() > max_asset_bytes {
        bail!(
            "decoded asset is {} bytes, exceeding --max-bytes={max_asset_bytes}",
            bytes.len()
        );
    }
    let content_hash = blake3::hash(&bytes).to_hex().to_string();
    write_private_file(output, &bytes, force)?;
    Ok(AssetExtractReport {
        asset,
        output: output.display().to_string(),
        byte_length: bytes.len(),
        content_hash,
    })
}

fn load_record(store: &Store, record_id: i64) -> Result<Record> {
    let mut statement = store.connection().prepare(
        "SELECT r.id, s.adapter, s.path, r.seq, r.byte_offset,
                r.byte_length, r.raw_hash, r.parse_status, r.parse_error, r.oversized
           FROM trace_records r
           JOIN trace_sources s ON s.id = r.source_id
          WHERE r.id = ?1",
    )?;
    statement
        .query_row(params![record_id], |row| {
            let parse_status: String = row.get(7)?;
            Ok(Record {
                id: row.get(0)?,
                source: RecordSource {
                    adapter: row.get(1)?,
                    uri: row.get(2)?,
                },
                seq: sql_u64(row.get(3)?, 3)?,
                byte_offset: sql_u64(row.get(4)?, 4)?,
                byte_length: sql_u64(row.get(5)?, 5)?,
                raw_hash: row.get(6)?,
                parse: RecordParse {
                    status: parse_status,
                    error: row.get(8)?,
                },
                oversized: row.get::<_, i64>(9)? != 0,
            })
        })
        .with_context(|| format!("unknown record {record_id:?}"))
}

fn sql_u64(value: i64, column: usize) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}

fn verify_loaded_record(record: &Record) -> Result<RecordVerification> {
    let mut file = match File::open(&record.source.uri) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(RecordVerification {
                record_id: record.id,
                status: "source_missing".to_owned(),
                expected_hash: record.raw_hash.clone(),
                actual_hash: None,
                expected_bytes: record.byte_length,
                actual_bytes: 0,
            });
        }
        Err(error) => {
            return Err(error).with_context(|| format!("failed to open {}", record.source.uri));
        }
    };
    file.seek(SeekFrom::Start(record.byte_offset))?;
    let mut limited = file.take(record.byte_length);
    let mut hasher = blake3::Hasher::new();
    let actual_bytes = std::io::copy(&mut limited, &mut hasher)?;
    let actual_hash = hasher.finalize().to_hex().to_string();
    let status = if actual_bytes != record.byte_length {
        "source_short"
    } else if actual_hash == record.raw_hash {
        "verified"
    } else {
        "hash_mismatch"
    };
    Ok(RecordVerification {
        record_id: record.id,
        status: status.to_owned(),
        expected_hash: record.raw_hash.clone(),
        actual_hash: Some(actual_hash),
        expected_bytes: record.byte_length,
        actual_bytes,
    })
}

fn read_record_bytes(record: &Record) -> Result<Vec<u8>> {
    let mut file = File::open(&record.source.uri)
        .with_context(|| format!("failed to open {}", record.source.uri))?;
    file.seek(SeekFrom::Start(record.byte_offset))?;
    let mut bytes = vec![0_u8; usize::try_from(record.byte_length)?];
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn externalize_value(
    value: &Value,
    record_id: i64,
    pointer: &str,
    max_value_bytes: usize,
    count: &mut usize,
) -> Value {
    match value {
        Value::String(text) => externalize_string(text, record_id, pointer, max_value_bytes, count),
        Value::Array(values) => Value::Array(
            values
                .iter()
                .enumerate()
                .map(|(index, value)| {
                    externalize_value(
                        value,
                        record_id,
                        &pointer_child(pointer, &index.to_string()),
                        max_value_bytes,
                        count,
                    )
                })
                .collect(),
        ),
        Value::Object(values) => Value::Object(
            values
                .iter()
                .map(|(key, value)| {
                    (
                        key.clone(),
                        externalize_value(
                            value,
                            record_id,
                            &pointer_child(pointer, key),
                            max_value_bytes,
                            count,
                        ),
                    )
                })
                .collect(),
        ),
        Value::Null | Value::Bool(_) | Value::Number(_) => value.clone(),
    }
}

fn externalize_string(
    text: &str,
    record_id: i64,
    pointer: &str,
    max_value_bytes: usize,
    count: &mut usize,
) -> Value {
    if let Some(data) = parse_data_uri(text) {
        *count += 1;
        return external_reference(
            "asset",
            record_id,
            pointer,
            Some(json!({
                "media_type": data.media_type,
                "encoding": "base64",
                "encoded_byte_length": data.encoded.len()
            })),
        );
    }
    if text.len() > max_value_bytes {
        *count += 1;
        return external_reference(
            "large_value",
            record_id,
            pointer,
            Some(json!({"byte_length": text.len()})),
        );
    }
    Value::String(text.to_owned())
}

fn external_reference(kind: &str, record_id: i64, pointer: &str, details: Option<Value>) -> Value {
    let reference = format!("{record_id}#{pointer}");
    let mut value = json!({
        "kind": kind,
        "reference": reference,
        "record_id": record_id,
        "json_pointer": pointer
    });
    if let (Some(object), Some(details)) = (value.as_object_mut(), details)
        && let Some(details) = details.as_object()
    {
        object.extend(details.clone());
    }
    json!({REFERENCE_KEY: value})
}

fn pointer_child(parent: &str, segment: &str) -> String {
    let escaped = segment.replace('~', "~0").replace('/', "~1");
    format!("{parent}/{escaped}")
}

struct DataUri<'a> {
    media_type: &'a str,
    encoded: &'a str,
}

fn parse_data_uri(value: &str) -> Option<DataUri<'_>> {
    let value = value.strip_prefix("data:")?;
    let (metadata, encoded) = value.split_once(',')?;
    let mut parts = metadata.split(';');
    let media_type = parts.next().filter(|value| !value.is_empty())?;
    parts
        .any(|value| value.eq_ignore_ascii_case("base64"))
        .then_some(DataUri {
            media_type,
            encoded,
        })
}

fn split_asset_reference(reference: &str) -> Result<(i64, &str)> {
    let (record_id, pointer) = reference
        .split_once('#')
        .context("asset reference must be <record-id>#<json-pointer>")?;
    if !pointer.is_empty() && !pointer.starts_with('/') {
        bail!("invalid asset reference {reference:?}");
    }
    let record_id = record_id
        .parse::<i64>()
        .with_context(|| format!("asset reference {reference:?} has a non-numeric record id"))?;
    Ok((record_id, pointer))
}

fn load_asset(
    store: &Store,
    reference: &str,
    max_record_bytes: u64,
) -> Result<(AssetDescriptor, String)> {
    let (record_id, pointer) = split_asset_reference(reference)?;
    let record = load_record(store, record_id)?;
    if record.byte_length > max_record_bytes {
        bail!(
            "record is {} bytes, exceeding --max-record-bytes={max_record_bytes}",
            record.byte_length
        );
    }
    let bytes = read_record_bytes(&record)?;
    let actual_hash = blake3::hash(&bytes).to_hex().to_string();
    if actual_hash != record.raw_hash {
        bail!(
            "record verification failed: expected {}, got {actual_hash}",
            record.raw_hash
        );
    }
    let value: Value = serde_json::from_slice(&bytes).context("record is not valid JSON")?;
    let text = value
        .pointer(pointer)
        .and_then(Value::as_str)
        .with_context(|| format!("asset pointer {pointer:?} does not resolve to a string"))?;
    let data = parse_data_uri(text).context("referenced value is not an inline base64 data URI")?;
    let descriptor = AssetDescriptor {
        reference: reference.to_owned(),
        record_id,
        json_pointer: pointer.to_owned(),
        media_type: data.media_type.to_owned(),
        encoding: "base64".to_owned(),
        encoded_byte_length: data.encoded.len(),
    };
    Ok((descriptor, data.encoded.to_owned()))
}

fn write_private_file(path: &Path, bytes: &[u8], force: bool) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true);
    if force {
        options.create(true).truncate(true);
    } else {
        options.create_new(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    if let Err(error) = file.write_all(bytes).and_then(|()| file.flush()) {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(error).with_context(|| format!("failed to write {}", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{externalize_value, parse_data_uri, pointer_child};
    use serde_json::json;

    #[test]
    fn externalizes_data_uris_and_large_values() {
        let value = json!({
            "image": "data:image/png;base64,aGVsbG8=",
            "text": "abcdefgh",
            "small": "ok"
        });
        let mut count = 0;
        let visible = externalize_value(&value, 42, "", 4, &mut count);
        assert_eq!(count, 2);
        assert_eq!(visible["image"]["$trace_ref"]["reference"], "42#/image");
        assert_eq!(visible["image"]["$trace_ref"]["media_type"], "image/png");
        assert_eq!(visible["text"]["$trace_ref"]["kind"], "large_value");
        assert_eq!(visible["small"], "ok");
    }

    #[test]
    fn recognizes_base64_data_uris_and_escapes_pointers() {
        let parsed =
            parse_data_uri("data:image/png;charset=utf-8;base64,aGVsbG8=").expect("data URI");
        assert_eq!(parsed.media_type, "image/png");
        assert_eq!(parsed.encoded, "aGVsbG8=");
        assert_eq!(pointer_child("/payload", "a/b~c"), "/payload/a~1b~0c");
    }
}
