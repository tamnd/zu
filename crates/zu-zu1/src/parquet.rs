//! Parquet edge lists behind the `arrow` feature.
//!
//! Arrow is interop only, never the internal representation (docs/12).
//! The reader takes the `src` and `dst` columns by name, falling back
//! to the first two columns when the names are absent, and accepts any
//! integer type that fits the u32 row domain; a null or an out-of-range
//! value is an error naming the column. The writer exists for
//! `zu convert`, so a SNAP text file can become the parquet input the
//! reader and the tests exercise.

use std::path::Path;

use arrow_array::{Array, RecordBatch, UInt32Array};
use arrow_schema::{DataType, Field, Schema};
use zu_common::{Result, ZuError};

use ::parquet::arrow::ArrowWriter;
use ::parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use ::parquet::basic::Compression;
use ::parquet::file::properties::WriterProperties;

/// Rows per record batch on both sides: big enough to amortize the
/// column plumbing, small enough to keep peak memory boring.
const BATCH_ROWS: usize = 1 << 20;

fn invalid(detail: String) -> ZuError {
    ZuError::InvalidArgument(detail)
}

/// Copies one integer column into the u32 row domain, rejecting nulls
/// and values past `u32::MAX` by column name.
fn column_to_u32(name: &str, array: &dyn Array, out: &mut Vec<u32>) -> Result<()> {
    if array.null_count() > 0 {
        return Err(invalid(format!("parquet column '{name}' holds nulls")));
    }
    macro_rules! widen {
        ($ty:ty) => {{
            let typed = array
                .as_any()
                .downcast_ref::<$ty>()
                .expect("downcast matches the data type");
            for i in 0..typed.len() {
                let v = i64::from(typed.value(i));
                let v = u32::try_from(v).map_err(|_| {
                    invalid(format!(
                        "parquet column '{name}' value {v} is outside the u32 row domain"
                    ))
                })?;
                out.push(v);
            }
        }};
    }
    match array.data_type() {
        DataType::UInt32 => widen!(arrow_array::UInt32Array),
        DataType::Int32 => widen!(arrow_array::Int32Array),
        DataType::Int64 => widen!(arrow_array::Int64Array),
        DataType::UInt64 => {
            let typed = array
                .as_any()
                .downcast_ref::<arrow_array::UInt64Array>()
                .expect("downcast matches the data type");
            for i in 0..typed.len() {
                let v = typed.value(i);
                let v = u32::try_from(v).map_err(|_| {
                    invalid(format!(
                        "parquet column '{name}' value {v} is outside the u32 row domain"
                    ))
                })?;
                out.push(v);
            }
        }
        other => {
            return Err(invalid(format!(
                "parquet column '{name}' has type {other}, expected an integer"
            )));
        }
    }
    Ok(())
}

/// Reads a `src,dst` parquet edge list into the same shape the SNAP and
/// csv readers produce.
pub fn read_edge_parquet(path: &Path) -> Result<Vec<(u32, u32)>> {
    let file = std::fs::File::open(path)?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)
        .map_err(|e| invalid(format!("parquet open: {e}")))?;
    let schema = builder.schema().clone();
    let find = |name: &str, fallback: usize| {
        schema
            .fields()
            .iter()
            .position(|f| f.name().eq_ignore_ascii_case(name))
            .unwrap_or(fallback)
    };
    if schema.fields().len() < 2 {
        return Err(invalid(format!(
            "parquet file has {} column(s), an edge list needs src and dst",
            schema.fields().len()
        )));
    }
    let (src_ix, dst_ix) = (find("src", 0), find("dst", 1));
    if src_ix == dst_ix {
        return Err(invalid(
            "parquet src and dst resolve to the same column".into(),
        ));
    }
    let reader = builder
        .with_batch_size(BATCH_ROWS)
        .build()
        .map_err(|e| invalid(format!("parquet open: {e}")))?;
    let (mut srcs, mut dsts) = (Vec::new(), Vec::new());
    for batch in reader {
        let batch = batch.map_err(|e| ZuError::Corrupt {
            what: "parquet batch",
            detail: e.to_string(),
        })?;
        column_to_u32("src", batch.column(src_ix).as_ref(), &mut srcs)?;
        column_to_u32("dst", batch.column(dst_ix).as_ref(), &mut dsts)?;
    }
    Ok(srcs.into_iter().zip(dsts).collect())
}

/// Writes edges as a two-column `src,dst` parquet file, snappy
/// compressed so the reader's codec path gets exercised by everything
/// `zu convert` produces.
pub fn write_edge_parquet(path: &Path, edges: &[(u32, u32)]) -> Result<()> {
    let schema = std::sync::Arc::new(Schema::new(vec![
        Field::new("src", DataType::UInt32, false),
        Field::new("dst", DataType::UInt32, false),
    ]));
    let file = std::fs::File::create(path)?;
    let props = WriterProperties::builder()
        .set_compression(Compression::SNAPPY)
        .build();
    let mut writer = ArrowWriter::try_new(file, schema.clone(), Some(props))
        .map_err(|e| invalid(format!("parquet create: {e}")))?;
    for chunk in edges.chunks(BATCH_ROWS) {
        let src = UInt32Array::from_iter_values(chunk.iter().map(|&(s, _)| s));
        let dst = UInt32Array::from_iter_values(chunk.iter().map(|&(_, d)| d));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![std::sync::Arc::new(src), std::sync::Arc::new(dst)],
        )
        .map_err(|e| invalid(format!("parquet batch: {e}")))?;
        writer
            .write(&batch)
            .map_err(|e| invalid(format!("parquet write: {e}")))?;
    }
    writer
        .close()
        .map_err(|e| invalid(format!("parquet close: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn roundtrip_matches_the_input() {
        let dir = temp();
        let path = dir.path().join("edges.parquet");
        let edges: Vec<(u32, u32)> = (0..300_000u32)
            .map(|i| (i / 7, i.wrapping_mul(2_654_435_761) % 999))
            .collect();
        write_edge_parquet(&path, &edges).expect("write");
        let back = read_edge_parquet(&path).expect("read");
        assert_eq!(back, edges);
    }

    #[test]
    fn named_columns_win_over_position() {
        // dst first, src second: the reader must pick by name, so the
        // roundtrip through a reordered schema still returns (src, dst).
        let dir = temp();
        let path = dir.path().join("swapped.parquet");
        let schema = std::sync::Arc::new(Schema::new(vec![
            Field::new("dst", DataType::UInt32, false),
            Field::new("src", DataType::UInt32, false),
        ]));
        let file = std::fs::File::create(&path).expect("create");
        let mut writer = ArrowWriter::try_new(file, schema.clone(), None).expect("writer");
        let batch = RecordBatch::try_new(
            schema,
            vec![
                std::sync::Arc::new(UInt32Array::from(vec![10u32, 20])),
                std::sync::Arc::new(UInt32Array::from(vec![1u32, 2])),
            ],
        )
        .expect("batch");
        writer.write(&batch).expect("write");
        writer.close().expect("close");
        assert_eq!(
            read_edge_parquet(&path).expect("read"),
            vec![(1, 10), (2, 20)]
        );
    }

    #[test]
    fn int64_columns_read_and_range_check() {
        let dir = temp();
        let path = dir.path().join("wide.parquet");
        let schema = std::sync::Arc::new(Schema::new(vec![
            Field::new("src", DataType::Int64, false),
            Field::new("dst", DataType::Int64, false),
        ]));
        let file = std::fs::File::create(&path).expect("create");
        let mut writer = ArrowWriter::try_new(file, schema.clone(), None).expect("writer");
        let batch = RecordBatch::try_new(
            schema,
            vec![
                std::sync::Arc::new(arrow_array::Int64Array::from(vec![3i64, 1 << 40])),
                std::sync::Arc::new(arrow_array::Int64Array::from(vec![4i64, 5])),
            ],
        )
        .expect("batch");
        writer.write(&batch).expect("write");
        writer.close().expect("close");
        let err = read_edge_parquet(&path).expect_err("out of range");
        let text = err.to_string();
        assert!(
            text.contains("src") && text.contains("u32"),
            "unexpected error: {text}"
        );
    }

    #[test]
    fn nulls_are_rejected_by_column_name() {
        let dir = temp();
        let path = dir.path().join("nulls.parquet");
        let schema = std::sync::Arc::new(Schema::new(vec![
            Field::new("src", DataType::UInt32, false),
            Field::new("dst", DataType::UInt32, true),
        ]));
        let file = std::fs::File::create(&path).expect("create");
        let mut writer = ArrowWriter::try_new(file, schema.clone(), None).expect("writer");
        let batch = RecordBatch::try_new(
            schema,
            vec![
                std::sync::Arc::new(UInt32Array::from(vec![1u32, 2])),
                std::sync::Arc::new(UInt32Array::from(vec![Some(7u32), None])),
            ],
        )
        .expect("batch");
        writer.write(&batch).expect("write");
        writer.close().expect("close");
        let err = read_edge_parquet(&path).expect_err("nulls");
        assert!(err.to_string().contains("dst"), "unexpected error: {err}");
    }

    #[test]
    fn one_column_file_is_refused() {
        let dir = temp();
        let path = dir.path().join("one.parquet");
        let schema = std::sync::Arc::new(Schema::new(vec![Field::new(
            "src",
            DataType::UInt32,
            false,
        )]));
        let file = std::fs::File::create(&path).expect("create");
        let mut writer = ArrowWriter::try_new(file, schema.clone(), None).expect("writer");
        let batch = RecordBatch::try_new(
            schema,
            vec![std::sync::Arc::new(UInt32Array::from(vec![1u32]))],
        )
        .expect("batch");
        writer.write(&batch).expect("write");
        writer.close().expect("close");
        let err = read_edge_parquet(&path).expect_err("one column");
        assert!(
            err.to_string().contains("needs src and dst"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn garbage_bytes_are_not_a_parquet_file() {
        let dir = temp();
        let path = dir.path().join("junk.parquet");
        std::fs::write(&path, b"not parquet at all").expect("write");
        assert!(read_edge_parquet(&path).is_err());
    }
}
