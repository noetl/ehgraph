//! Arrow IPC images of nodes and edges.
//!
//! Dynamic properties are a JSON column. The fixed columns are the soft-history and
//! provenance fields the server always has. Session graphs and snapshots share this layout.

use std::io::Cursor;
use std::sync::Arc;

use arrow_array::{Array, BooleanArray, Int64Array, RecordBatch, StringArray, UInt64Array};
use arrow_ipc::reader::FileReader;
use arrow_ipc::writer::FileWriter;
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use ehgraph_core::{Edge, EhError, Node, PropValue};

pub fn node_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new("type", DataType::Utf8, false),
        Field::new("props_json", DataType::Utf8, false),
        Field::new("first_seen", DataType::Int64, false),
        Field::new("last_seen", DataType::Int64, false),
        Field::new("active", DataType::Boolean, false),
        Field::new("origin", DataType::Utf8, true),
        Field::new("source", DataType::Utf8, false),
        Field::new("execution_id", DataType::Utf8, false),
        Field::new("version_created", DataType::UInt64, false),
        Field::new("version_updated", DataType::UInt64, false),
    ]))
}

pub fn edge_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new("src", DataType::Utf8, false),
        Field::new("dst", DataType::Utf8, false),
        Field::new("type", DataType::Utf8, false),
        Field::new("props_json", DataType::Utf8, false),
        Field::new("first_seen", DataType::Int64, false),
        Field::new("last_seen", DataType::Int64, false),
        Field::new("active", DataType::Boolean, false),
        Field::new("origin", DataType::Utf8, true),
        Field::new("source", DataType::Utf8, false),
        Field::new("execution_id", DataType::Utf8, false),
        Field::new("version_created", DataType::UInt64, false),
        Field::new("version_updated", DataType::UInt64, false),
    ]))
}

pub fn nodes_to_batch(nodes: &[Node]) -> Result<RecordBatch, EhError> {
    let schema = node_schema();
    let props = props_column(nodes.iter().map(|node| &node.props))?;
    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(StringArray::from(
                nodes.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                nodes
                    .iter()
                    .map(|n| n.type_name.as_str())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(props),
            Arc::new(Int64Array::from(
                nodes.iter().map(|n| n.first_seen).collect::<Vec<_>>(),
            )),
            Arc::new(Int64Array::from(
                nodes.iter().map(|n| n.last_seen).collect::<Vec<_>>(),
            )),
            Arc::new(BooleanArray::from(
                nodes.iter().map(|n| n.active).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                nodes
                    .iter()
                    .map(|n| n.origin.clone())
                    .collect::<Vec<Option<String>>>(),
            )),
            Arc::new(StringArray::from(
                nodes.iter().map(|n| n.source.as_str()).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                nodes
                    .iter()
                    .map(|n| n.execution_id.as_str())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(UInt64Array::from(
                nodes.iter().map(|n| n.version_created).collect::<Vec<_>>(),
            )),
            Arc::new(UInt64Array::from(
                nodes.iter().map(|n| n.version_updated).collect::<Vec<_>>(),
            )),
        ],
    )
    .map_err(arrow_err)?;
    Ok(batch)
}

pub fn edges_to_batch(edges: &[Edge]) -> Result<RecordBatch, EhError> {
    let schema = edge_schema();
    let props = props_column(edges.iter().map(|edge| &edge.props))?;
    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(StringArray::from(
                edges.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                edges.iter().map(|e| e.src.as_str()).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                edges.iter().map(|e| e.dst.as_str()).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                edges
                    .iter()
                    .map(|e| e.type_name.as_str())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(props),
            Arc::new(Int64Array::from(
                edges.iter().map(|e| e.first_seen).collect::<Vec<_>>(),
            )),
            Arc::new(Int64Array::from(
                edges.iter().map(|e| e.last_seen).collect::<Vec<_>>(),
            )),
            Arc::new(BooleanArray::from(
                edges.iter().map(|e| e.active).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                edges
                    .iter()
                    .map(|e| e.origin.clone())
                    .collect::<Vec<Option<String>>>(),
            )),
            Arc::new(StringArray::from(
                edges.iter().map(|e| e.source.as_str()).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                edges
                    .iter()
                    .map(|e| e.execution_id.as_str())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(UInt64Array::from(
                edges.iter().map(|e| e.version_created).collect::<Vec<_>>(),
            )),
            Arc::new(UInt64Array::from(
                edges.iter().map(|e| e.version_updated).collect::<Vec<_>>(),
            )),
        ],
    )
    .map_err(arrow_err)?;
    Ok(batch)
}

fn props_column<'a>(
    props: impl Iterator<Item = &'a std::collections::BTreeMap<String, PropValue>>,
) -> Result<StringArray, EhError> {
    let mut values = Vec::new();
    for map in props {
        values.push(serde_json::to_string(map)?);
    }
    Ok(StringArray::from(values))
}

pub fn batch_to_nodes(batch: &RecordBatch) -> Result<Vec<Node>, EhError> {
    let id = strings(batch, "id")?;
    let type_name = strings(batch, "type")?;
    let props_json = strings(batch, "props_json")?;
    let first_seen = i64s(batch, "first_seen")?;
    let last_seen = i64s(batch, "last_seen")?;
    let active = bools(batch, "active")?;
    let origin = opt_strings(batch, "origin")?;
    let source = strings(batch, "source")?;
    let execution_id = strings(batch, "execution_id")?;
    let version_created = u64s(batch, "version_created")?;
    let version_updated = u64s(batch, "version_updated")?;
    let mut nodes = Vec::with_capacity(batch.num_rows());
    for i in 0..batch.num_rows() {
        let props = serde_json::from_str(props_json.value(i))?;
        nodes.push(Node {
            id: id.value(i).to_string(),
            type_name: type_name.value(i).to_string(),
            props,
            first_seen: first_seen.value(i),
            last_seen: last_seen.value(i),
            active: active.value(i),
            origin: opt_at(origin, i),
            source: source.value(i).to_string(),
            execution_id: execution_id.value(i).to_string(),
            version_created: version_created.value(i),
            version_updated: version_updated.value(i),
        });
    }
    Ok(nodes)
}

pub fn batch_to_edges(batch: &RecordBatch) -> Result<Vec<Edge>, EhError> {
    let id = strings(batch, "id")?;
    let src = strings(batch, "src")?;
    let dst = strings(batch, "dst")?;
    let type_name = strings(batch, "type")?;
    let props_json = strings(batch, "props_json")?;
    let first_seen = i64s(batch, "first_seen")?;
    let last_seen = i64s(batch, "last_seen")?;
    let active = bools(batch, "active")?;
    let origin = opt_strings(batch, "origin")?;
    let source = strings(batch, "source")?;
    let execution_id = strings(batch, "execution_id")?;
    let version_created = u64s(batch, "version_created")?;
    let version_updated = u64s(batch, "version_updated")?;
    let mut edges = Vec::with_capacity(batch.num_rows());
    for i in 0..batch.num_rows() {
        let props = serde_json::from_str(props_json.value(i))?;
        edges.push(Edge {
            id: id.value(i).to_string(),
            src: src.value(i).to_string(),
            dst: dst.value(i).to_string(),
            type_name: type_name.value(i).to_string(),
            props,
            first_seen: first_seen.value(i),
            last_seen: last_seen.value(i),
            active: active.value(i),
            origin: opt_at(origin, i),
            source: source.value(i).to_string(),
            execution_id: execution_id.value(i).to_string(),
            version_created: version_created.value(i),
            version_updated: version_updated.value(i),
        });
    }
    Ok(edges)
}

pub fn batches_to_ipc(batch: &RecordBatch) -> Result<Vec<u8>, EhError> {
    let mut cursor = Cursor::new(Vec::new());
    {
        let mut writer = FileWriter::try_new(&mut cursor, &batch.schema()).map_err(arrow_err)?;
        writer.write(batch).map_err(arrow_err)?;
        writer.finish().map_err(arrow_err)?;
    }
    Ok(cursor.into_inner())
}

pub fn ipc_to_batches(bytes: &[u8]) -> Result<Vec<RecordBatch>, EhError> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let cursor = Cursor::new(bytes);
    let reader = FileReader::try_new(cursor, None).map_err(arrow_err)?;
    let mut out = Vec::new();
    for batch in reader {
        out.push(batch.map_err(arrow_err)?);
    }
    Ok(out)
}

pub fn ipc_to_nodes(bytes: &[u8]) -> Result<Vec<Node>, EhError> {
    let mut nodes = Vec::new();
    for batch in ipc_to_batches(bytes)? {
        nodes.extend(batch_to_nodes(&batch)?);
    }
    Ok(nodes)
}

pub fn ipc_to_edges(bytes: &[u8]) -> Result<Vec<Edge>, EhError> {
    let mut edges = Vec::new();
    for batch in ipc_to_batches(bytes)? {
        edges.extend(batch_to_edges(&batch)?);
    }
    Ok(edges)
}

pub fn batch_memory_size(batch: &RecordBatch) -> u64 {
    batch
        .columns()
        .iter()
        .map(|column| column.get_array_memory_size() as u64)
        .sum()
}

fn strings<'a>(batch: &'a RecordBatch, name: &str) -> Result<&'a StringArray, EhError> {
    downcast(batch, name)
}

fn opt_strings<'a>(batch: &'a RecordBatch, name: &str) -> Result<&'a StringArray, EhError> {
    downcast(batch, name)
}

fn i64s<'a>(batch: &'a RecordBatch, name: &str) -> Result<&'a Int64Array, EhError> {
    downcast(batch, name)
}

fn u64s<'a>(batch: &'a RecordBatch, name: &str) -> Result<&'a UInt64Array, EhError> {
    downcast(batch, name)
}

fn bools<'a>(batch: &'a RecordBatch, name: &str) -> Result<&'a BooleanArray, EhError> {
    downcast(batch, name)
}

fn downcast<'a, T: Array + 'static>(batch: &'a RecordBatch, name: &str) -> Result<&'a T, EhError> {
    let column = batch
        .column_by_name(name)
        .ok_or_else(|| EhError::unavailable(format!("arrow batch missing column {name}")))?;
    column
        .as_any()
        .downcast_ref::<T>()
        .ok_or_else(|| EhError::unavailable(format!("arrow column {name} has an unexpected type")))
}

fn opt_at(array: &StringArray, index: usize) -> Option<String> {
    if array.is_null(index) {
        None
    } else {
        Some(array.value(index).to_string())
    }
}

fn arrow_err(err: impl std::fmt::Display) -> EhError {
    EhError::unavailable(format!("arrow: {err}"))
}
