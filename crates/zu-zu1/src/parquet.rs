//! Parquet edge lists behind the `arrow` feature.
//!
//! Arrow is interop only, never the internal representation (docs/12).
//! The reader takes the `src` and `dst` columns by name, falling back
//! to the first two columns when the names are absent, and accepts any
//! integer type that fits the u32 row domain; a null or an out-of-range
//! value is an error naming the column. The writer exists for
//! `zu convert`, so a SNAP text file can become the parquet input the
//! reader and the tests exercise. A column that is neither src nor dst
//! is an edge property column when the caller asks for one, taken by
//! the name the file gives it.

use std::path::Path;

use arrow_array::{Array, RecordBatch, UInt32Array};
use arrow_schema::{DataType, Field, Schema};
use zu_common::{Result, ZuError};

use crate::props::{EdgesWithProps, OwnedColumn, OwnedValues};

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

/// The owned column an edge property column of `ty` accumulates into,
/// or `None` for a type the row format has no home for.
fn owned_for(ty: &DataType) -> Option<OwnedValues> {
    match ty {
        DataType::Int8
        | DataType::Int16
        | DataType::Int32
        | DataType::Int64
        | DataType::UInt8
        | DataType::UInt16
        | DataType::UInt32
        | DataType::UInt64 => Some(OwnedValues::Int(Vec::new())),
        DataType::Float32 | DataType::Float64 => Some(OwnedValues::Float(Vec::new())),
        DataType::Boolean => Some(OwnedValues::Bool(Vec::new())),
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Binary | DataType::LargeBinary => {
            Some(OwnedValues::Str(Vec::new()))
        }
        _ => None,
    }
}

/// Appends one batch of one property column to the column it belongs
/// to, in the file's row order.
///
/// A signed integer keeps its bits rather than its value, which is what
/// the lane holds and what the reader undoes by type. A null is refused
/// by name: the store this feeds takes dense columns, and a parquet
/// edge list that means absent has to say so through the staging path
/// that carries validity.
fn append_column(name: &str, array: &dyn Array, into: &mut OwnedValues) -> Result<()> {
    if array.null_count() > 0 {
        return Err(invalid(format!(
            "parquet column '{name}' holds nulls, and an edge property column loaded this way \
             is dense"
        )));
    }
    macro_rules! ints {
        ($ty:ty, $out:expr) => {{
            let typed = array
                .as_any()
                .downcast_ref::<$ty>()
                .expect("downcast matches the data type");
            for i in 0..typed.len() {
                $out.push(i64::from(typed.value(i)) as u64);
            }
        }};
    }
    macro_rules! bytes {
        ($ty:ty, $out:expr) => {{
            let typed = array
                .as_any()
                .downcast_ref::<$ty>()
                .expect("downcast matches the data type");
            for i in 0..typed.len() {
                $out.push(typed.value(i).as_bytes().to_vec());
            }
        }};
    }
    match (into, array.data_type()) {
        (OwnedValues::Int(out), DataType::Int8) => ints!(arrow_array::Int8Array, out),
        (OwnedValues::Int(out), DataType::Int16) => ints!(arrow_array::Int16Array, out),
        (OwnedValues::Int(out), DataType::Int32) => ints!(arrow_array::Int32Array, out),
        (OwnedValues::Int(out), DataType::UInt8) => ints!(arrow_array::UInt8Array, out),
        (OwnedValues::Int(out), DataType::UInt16) => ints!(arrow_array::UInt16Array, out),
        (OwnedValues::Int(out), DataType::UInt32) => ints!(arrow_array::UInt32Array, out),
        (OwnedValues::Int(out), DataType::Int64) => {
            let typed = array
                .as_any()
                .downcast_ref::<arrow_array::Int64Array>()
                .expect("downcast matches the data type");
            for i in 0..typed.len() {
                out.push(typed.value(i) as u64);
            }
        }
        (OwnedValues::Int(out), DataType::UInt64) => {
            let typed = array
                .as_any()
                .downcast_ref::<arrow_array::UInt64Array>()
                .expect("downcast matches the data type");
            out.extend(typed.values().iter().copied());
        }
        (OwnedValues::Float(out), DataType::Float32) => {
            let typed = array
                .as_any()
                .downcast_ref::<arrow_array::Float32Array>()
                .expect("downcast matches the data type");
            out.extend(typed.values().iter().map(|&v| f64::from(v)));
        }
        (OwnedValues::Float(out), DataType::Float64) => {
            let typed = array
                .as_any()
                .downcast_ref::<arrow_array::Float64Array>()
                .expect("downcast matches the data type");
            out.extend(typed.values().iter().copied());
        }
        (OwnedValues::Bool(out), DataType::Boolean) => {
            let typed = array
                .as_any()
                .downcast_ref::<arrow_array::BooleanArray>()
                .expect("downcast matches the data type");
            out.extend((0..typed.len()).map(|i| typed.value(i)));
        }
        (OwnedValues::Str(out), DataType::Utf8) => bytes!(arrow_array::StringArray, out),
        (OwnedValues::Str(out), DataType::LargeUtf8) => bytes!(arrow_array::LargeStringArray, out),
        (OwnedValues::Str(out), DataType::Binary) => {
            let typed = array
                .as_any()
                .downcast_ref::<arrow_array::BinaryArray>()
                .expect("downcast matches the data type");
            for i in 0..typed.len() {
                out.push(typed.value(i).to_vec());
            }
        }
        (OwnedValues::Str(out), DataType::LargeBinary) => {
            let typed = array
                .as_any()
                .downcast_ref::<arrow_array::LargeBinaryArray>()
                .expect("downcast matches the data type");
            for i in 0..typed.len() {
                out.push(typed.value(i).to_vec());
            }
        }
        // The accumulator was chosen from this same schema, so a batch
        // whose type disagrees with it is a file that changed shape
        // between its metadata and its pages.
        (_, other) => {
            return Err(ZuError::Corrupt {
                what: "parquet batch",
                detail: format!("column '{name}' arrives as {other}, which the schema did not say"),
            });
        }
    }
    Ok(())
}

/// Reads a `src,dst` parquet edge list into the same shape the SNAP and
/// csv readers produce, dropping any other column.
pub fn read_edge_parquet(path: &Path) -> Result<Vec<(u32, u32)>> {
    Ok(read_edge_parquet_with_props(path, false)?.0)
}

/// Reads a parquet edge list, and with `props` set every column that is
/// neither src nor dst as an edge property column of the same name.
///
/// The columns come back in the file's row order, which is the order
/// the edge list comes back in, so a caller that sorts one has to move
/// the other by the same permutation. `reorder::load_order` is what
/// hands that permutation over.
pub fn read_edge_parquet_with_props(path: &Path, props: bool) -> Result<EdgesWithProps> {
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
    let mut columns: Vec<(usize, OwnedColumn)> = Vec::new();
    if props {
        for (ix, field) in schema.fields().iter().enumerate() {
            if ix == src_ix || ix == dst_ix {
                continue;
            }
            let values = owned_for(field.data_type()).ok_or_else(|| {
                invalid(format!(
                    "parquet column '{}' has type {}, which is not an edge property type",
                    field.name(),
                    field.data_type()
                ))
            })?;
            columns.push((
                ix,
                OwnedColumn {
                    name: field.name().clone(),
                    values,
                },
            ));
        }
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
        for (ix, column) in &mut columns {
            append_column(&column.name, batch.column(*ix).as_ref(), &mut column.values)?;
        }
    }
    let edges: Vec<(u32, u32)> = srcs.into_iter().zip(dsts).collect();
    Ok((edges, columns.into_iter().map(|(_, c)| c).collect()))
}

/// Writes edges as a two-column `src,dst` parquet file, snappy
/// compressed so the reader's codec path gets exercised by everything
/// `zu convert` produces.
pub fn write_edge_parquet(path: &Path, edges: &[(u32, u32)]) -> Result<()> {
    write_edge_parquet_with_props(path, edges, &[])
}

/// The same writer, carrying one column per edge property, which the
/// reader takes back by name.
///
/// Every column has to hold one value per edge, in the same order the
/// edges are in, because that pairing is the only thing that says which
/// value belongs to which edge.
pub fn write_edge_parquet_with_props(
    path: &Path,
    edges: &[(u32, u32)],
    columns: &[OwnedColumn],
) -> Result<()> {
    for column in columns {
        if column.values.len() != edges.len() {
            return Err(invalid(format!(
                "column '{}' holds {} value(s) for {} edge(s)",
                column.name,
                column.values.len(),
                edges.len()
            )));
        }
    }
    let mut fields = vec![
        Field::new("src", DataType::UInt32, false),
        Field::new("dst", DataType::UInt32, false),
    ];
    for column in columns {
        let ty = match column.values {
            OwnedValues::Int(_) => DataType::Int64,
            OwnedValues::Float(_) => DataType::Float64,
            OwnedValues::Bool(_) => DataType::Boolean,
            OwnedValues::Str(_) => DataType::Binary,
        };
        fields.push(Field::new(&column.name, ty, false));
    }
    let schema = std::sync::Arc::new(Schema::new(fields));
    let file = std::fs::File::create(path)?;
    let props = WriterProperties::builder()
        .set_compression(Compression::SNAPPY)
        .build();
    let mut writer = ArrowWriter::try_new(file, schema.clone(), Some(props))
        .map_err(|e| invalid(format!("parquet create: {e}")))?;
    for (chunk_ix, chunk) in edges.chunks(BATCH_ROWS).enumerate() {
        let lo = chunk_ix * BATCH_ROWS;
        let rows = lo..lo + chunk.len();
        let src = UInt32Array::from_iter_values(chunk.iter().map(|&(s, _)| s));
        let dst = UInt32Array::from_iter_values(chunk.iter().map(|&(_, d)| d));
        let mut arrays: Vec<std::sync::Arc<dyn Array>> =
            vec![std::sync::Arc::new(src), std::sync::Arc::new(dst)];
        for column in columns {
            let array: std::sync::Arc<dyn Array> = match &column.values {
                OwnedValues::Int(v) => {
                    std::sync::Arc::new(arrow_array::Int64Array::from_iter_values(
                        v[rows.clone()].iter().map(|&w| w as i64),
                    ))
                }
                OwnedValues::Float(v) => std::sync::Arc::new(
                    arrow_array::Float64Array::from_iter_values(v[rows.clone()].iter().copied()),
                ),
                OwnedValues::Bool(v) => {
                    std::sync::Arc::new(arrow_array::BooleanArray::from(v[rows.clone()].to_vec()))
                }
                OwnedValues::Str(v) => std::sync::Arc::new(
                    arrow_array::BinaryArray::from_iter_values(v[rows.clone()].iter()),
                ),
            };
            arrays.push(array);
        }
        let batch = RecordBatch::try_new(schema.clone(), arrays)
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

    /// Writes a two edge file with one column of each property type,
    /// deliberately not in load order so the caller has to sort.
    fn props_file(path: &Path) {
        let schema = std::sync::Arc::new(Schema::new(vec![
            Field::new("src", DataType::UInt32, false),
            Field::new("dst", DataType::UInt32, false),
            Field::new("since", DataType::Int64, false),
            Field::new("weight", DataType::Float64, false),
            Field::new("live", DataType::Boolean, false),
            Field::new("note", DataType::Utf8, false),
        ]));
        let file = std::fs::File::create(path).expect("create");
        let mut writer = ArrowWriter::try_new(file, schema.clone(), None).expect("writer");
        let batch = RecordBatch::try_new(
            schema,
            vec![
                std::sync::Arc::new(UInt32Array::from(vec![4u32, 1])),
                std::sync::Arc::new(UInt32Array::from(vec![0u32, 2])),
                std::sync::Arc::new(arrow_array::Int64Array::from(vec![-7i64, 9])),
                std::sync::Arc::new(arrow_array::Float64Array::from(vec![0.5f64, 1.5])),
                std::sync::Arc::new(arrow_array::BooleanArray::from(vec![true, false])),
                std::sync::Arc::new(arrow_array::StringArray::from(vec!["four", "one"])),
            ],
        )
        .expect("batch");
        writer.write(&batch).expect("write");
        writer.close().expect("close");
    }

    #[test]
    fn every_other_column_is_an_edge_property() {
        let dir = temp();
        let path = dir.path().join("props.parquet");
        props_file(&path);
        let (edges, columns) = read_edge_parquet_with_props(&path, true).expect("read");
        assert_eq!(edges, vec![(4, 0), (1, 2)]);
        let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["since", "weight", "live", "note"]);
        // A signed value keeps its bits: the lane holds a word and the
        // column's type is what says how to read it.
        assert_eq!(
            columns[0].values,
            OwnedValues::Int(vec![-7i64 as u64, 9u64])
        );
        assert_eq!(columns[1].values, OwnedValues::Float(vec![0.5, 1.5]));
        assert_eq!(columns[2].values, OwnedValues::Bool(vec![true, false]));
        assert_eq!(
            columns[3].values,
            OwnedValues::Str(vec![b"four".to_vec(), b"one".to_vec()])
        );
        // The same file read as a bare edge list answers the same edges
        // and says nothing about the columns.
        assert_eq!(read_edge_parquet(&path).expect("read"), edges);
    }

    #[test]
    fn a_property_column_moves_with_the_load_order() {
        let dir = temp();
        let path = dir.path().join("props.parquet");
        props_file(&path);
        let (mut edges, columns) = read_edge_parquet_with_props(&path, true).expect("read");
        let order = crate::reorder::load_order(&mut edges);
        assert_eq!(edges, vec![(1, 2), (4, 0)]);
        assert_eq!(order, vec![1, 0]);
        let moved: Vec<OwnedValues> = columns.iter().map(|c| c.values.permuted(&order)).collect();
        assert_eq!(moved[0], OwnedValues::Int(vec![9u64, -7i64 as u64]));
        assert_eq!(
            moved[3],
            OwnedValues::Str(vec![b"one".to_vec(), b"four".to_vec()])
        );
    }

    #[test]
    fn columns_written_come_back_paired_with_their_edges() {
        let dir = temp();
        let path = dir.path().join("written.parquet");
        let edges = vec![(9u32, 1u32), (0, 4), (5, 5)];
        let columns = vec![
            OwnedColumn {
                name: "since".into(),
                values: OwnedValues::Int(vec![1, 2, 3]),
            },
            OwnedColumn {
                name: "note".into(),
                values: OwnedValues::Str(vec![b"a".to_vec(), b"bb".to_vec(), b"ccc".to_vec()]),
            },
        ];
        write_edge_parquet_with_props(&path, &edges, &columns).expect("write");
        let (back, back_columns) = read_edge_parquet_with_props(&path, true).expect("read");
        assert_eq!(back, edges);
        assert_eq!(back_columns, columns);
    }

    #[test]
    fn a_column_short_of_a_value_per_edge_is_refused() {
        let dir = temp();
        let path = dir.path().join("short.parquet");
        let columns = vec![OwnedColumn {
            name: "since".into(),
            values: OwnedValues::Int(vec![1]),
        }];
        let err = write_edge_parquet_with_props(&path, &[(0, 1), (1, 2)], &columns)
            .expect_err("short column");
        assert!(
            err.to_string().contains("1 value(s) for 2 edge(s)"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn a_property_type_the_row_format_has_no_home_for_is_refused() {
        let dir = temp();
        let path = dir.path().join("odd.parquet");
        let schema = std::sync::Arc::new(Schema::new(vec![
            Field::new("src", DataType::UInt32, false),
            Field::new("dst", DataType::UInt32, false),
            Field::new(
                "when",
                DataType::Interval(arrow_schema::IntervalUnit::YearMonth),
                false,
            ),
        ]));
        let file = std::fs::File::create(&path).expect("create");
        let mut writer = ArrowWriter::try_new(file, schema.clone(), None).expect("writer");
        let batch = RecordBatch::try_new(
            schema,
            vec![
                std::sync::Arc::new(UInt32Array::from(vec![1u32])),
                std::sync::Arc::new(UInt32Array::from(vec![2u32])),
                std::sync::Arc::new(arrow_array::IntervalYearMonthArray::from(vec![3i32])),
            ],
        )
        .expect("batch");
        writer.write(&batch).expect("write");
        writer.close().expect("close");
        let err = read_edge_parquet_with_props(&path, true).expect_err("interval");
        assert!(
            err.to_string().contains("'when'") && err.to_string().contains("not an edge property"),
            "unexpected error: {err}"
        );
        // The same file is a perfectly good edge list when nobody asks
        // for its other columns.
        assert_eq!(read_edge_parquet(&path).expect("read"), vec![(1u32, 2u32)]);
    }

    #[test]
    fn a_null_in_a_property_column_is_refused_by_name() {
        let dir = temp();
        let path = dir.path().join("nullprop.parquet");
        let schema = std::sync::Arc::new(Schema::new(vec![
            Field::new("src", DataType::UInt32, false),
            Field::new("dst", DataType::UInt32, false),
            Field::new("since", DataType::Int64, true),
        ]));
        let file = std::fs::File::create(&path).expect("create");
        let mut writer = ArrowWriter::try_new(file, schema.clone(), None).expect("writer");
        let batch = RecordBatch::try_new(
            schema,
            vec![
                std::sync::Arc::new(UInt32Array::from(vec![1u32, 2])),
                std::sync::Arc::new(UInt32Array::from(vec![2u32, 3])),
                std::sync::Arc::new(arrow_array::Int64Array::from(vec![Some(1i64), None])),
            ],
        )
        .expect("batch");
        writer.write(&batch).expect("write");
        writer.close().expect("close");
        let err = read_edge_parquet_with_props(&path, true).expect_err("null");
        assert!(err.to_string().contains("'since'"), "unexpected: {err}");
    }

    #[test]
    fn garbage_bytes_are_not_a_parquet_file() {
        let dir = temp();
        let path = dir.path().join("junk.parquet");
        std::fs::write(&path, b"not parquet at all").expect("write");
        assert!(read_edge_parquet(&path).is_err());
    }
}
