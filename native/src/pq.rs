//! A small Parquet writer: PLAIN, uncompressed, one data page per column per
//! row group. Values are appended already encoded, so a flush is a few copies.

use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    Text,
    OptionalText,
    Date,
    Int32,
    Int64,
}

struct Column {
    name: &'static str,
    kind: Kind,
    values: Vec<u8>,
    /// One bit per row for `OptionalText`: 1 where the value is present.
    present: Vec<u8>,
}

struct ChunkMeta {
    offset: u64,
    size: u64,
}

struct GroupMeta {
    rows: u64,
    chunks: Vec<ChunkMeta>,
}

pub struct Writer {
    file: File,
    columns: Vec<Column>,
    rows: usize,
    group_rows: usize,
    offset: u64,
    groups: Vec<GroupMeta>,
    scratch: Vec<u8>,
}

/// Thrift compact protocol, as far as the Parquet footer needs it.
struct Thrift {
    out: Vec<u8>,
    last: i16,
    stack: Vec<i16>,
}

impl Thrift {
    fn new() -> Self {
        Self {
            out: Vec::new(),
            last: 0,
            stack: Vec::new(),
        }
    }

    fn varint(&mut self, mut value: u64) {
        while value >= 0x80 {
            self.out.push(value as u8 | 0x80);
            value >>= 7;
        }
        self.out.push(value as u8);
    }

    fn zigzag(&mut self, value: i64) {
        self.varint(((value << 1) ^ (value >> 63)) as u64);
    }

    fn field(&mut self, id: i16, kind: u8) {
        let delta = id - self.last;
        if (1..=15).contains(&delta) {
            self.out.push(((delta as u8) << 4) | kind);
        } else {
            self.out.push(kind);
            self.zigzag(id as i64);
        }
        self.last = id;
    }

    fn i32(&mut self, id: i16, value: i32) {
        self.field(id, 5);
        self.zigzag(value as i64);
    }

    fn i64(&mut self, id: i16, value: i64) {
        self.field(id, 6);
        self.zigzag(value);
    }

    fn text(&mut self, id: i16, value: &str) {
        self.field(id, 8);
        self.varint(value.len() as u64);
        self.out.extend_from_slice(value.as_bytes());
    }

    fn list(&mut self, id: i16, element: u8, count: usize) {
        self.field(id, 9);
        if count < 15 {
            self.out.push(((count as u8) << 4) | element);
        } else {
            self.out.push(0xF0 | element);
            self.varint(count as u64);
        }
    }

    /// A struct as a field (`id` > 0) or as a list element (`id` == 0).
    fn begin(&mut self, id: i16) {
        if id > 0 {
            self.field(id, 12);
        }
        self.stack.push(self.last);
        self.last = 0;
    }

    fn end(&mut self) {
        self.out.push(0);
        self.last = self.stack.pop().unwrap();
    }
}

impl Writer {
    pub fn create(path: &Path, columns: &[(&'static str, Kind)], group_rows: usize) -> io::Result<Self> {
        let mut file = File::create(path)?;
        file.write_all(b"PAR1")?;
        Ok(Self {
            file,
            columns: columns
                .iter()
                .map(|&(name, kind)| Column {
                    name,
                    kind,
                    values: Vec::new(),
                    present: Vec::new(),
                })
                .collect(),
            rows: 0,
            group_rows,
            offset: 4,
            groups: Vec::new(),
            scratch: Vec::new(),
        })
    }

    #[inline(always)]
    pub fn text(&mut self, column: usize, value: &[u8]) {
        let out = &mut self.columns[column].values;
        out.extend_from_slice(&(value.len() as u32).to_le_bytes());
        out.extend_from_slice(value);
    }

    /// The encoded values of a text column, for callers that append in place:
    /// each value is its length as 4 little-endian bytes, then its bytes.
    #[inline(always)]
    pub fn raw(&mut self, column: usize) -> &mut Vec<u8> {
        &mut self.columns[column].values
    }

    #[inline(always)]
    pub fn optional_text(&mut self, column: usize, value: Option<&[u8]>) {
        let row = self.rows;
        let column = &mut self.columns[column];
        if row & 7 == 0 {
            column.present.push(0);
        }
        if let Some(value) = value {
            *column.present.last_mut().unwrap() |= 1 << (row & 7);
            column.values.extend_from_slice(&(value.len() as u32).to_le_bytes());
            column.values.extend_from_slice(value);
        }
    }

    #[inline(always)]
    pub fn int32(&mut self, column: usize, value: i32) {
        self.columns[column].values.extend_from_slice(&value.to_le_bytes());
    }

    #[inline(always)]
    pub fn int64(&mut self, column: usize, value: i64) {
        self.columns[column].values.extend_from_slice(&value.to_le_bytes());
    }

    #[inline(always)]
    pub fn end_row(&mut self) -> io::Result<()> {
        self.rows += 1;
        if self.rows >= self.group_rows {
            self.flush()
        } else {
            Ok(())
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.rows == 0 {
            return Ok(());
        }
        let rows = self.rows;
        let mut out = std::mem::take(&mut self.scratch);
        out.clear();
        let mut chunks = Vec::with_capacity(self.columns.len());
        for column in &mut self.columns {
            let mut levels = Vec::new();
            if column.kind == Kind::OptionalText {
                // Definition levels: one bit-packed run of width 1.
                let groups = rows.div_ceil(8);
                let mut run = Thrift::new();
                run.varint(((groups as u64) << 1) | 1);
                levels.extend_from_slice(&((run.out.len() + groups) as u32).to_le_bytes());
                levels.extend_from_slice(&run.out);
                levels.extend_from_slice(&column.present[..groups]);
            }
            let body = levels.len() + column.values.len();
            let mut header = Thrift::new();
            header.i32(1, 0); // DATA_PAGE
            header.i32(2, body as i32);
            header.i32(3, body as i32);
            header.begin(5);
            header.i32(1, rows as i32);
            header.i32(2, 0); // PLAIN
            header.i32(3, 3); // RLE
            header.i32(4, 3); // RLE
            header.end();
            header.out.push(0);
            let start = out.len();
            out.extend_from_slice(&header.out);
            out.extend_from_slice(&levels);
            out.extend_from_slice(&column.values);
            chunks.push(ChunkMeta {
                offset: self.offset + start as u64,
                size: (out.len() - start) as u64,
            });
            column.values.clear();
            column.present.clear();
        }
        self.file.write_all(&out)?;
        self.offset += out.len() as u64;
        self.scratch = out;
        self.groups.push(GroupMeta {
            rows: rows as u64,
            chunks,
        });
        self.rows = 0;
        Ok(())
    }

    pub fn finish(mut self) -> io::Result<()> {
        self.flush()?;
        let mut meta = Thrift::new();
        meta.i32(1, 1);
        meta.list(2, 12, self.columns.len() + 1);
        meta.begin(0);
        meta.text(4, "schema");
        meta.i32(5, self.columns.len() as i32);
        meta.end();
        for column in &self.columns {
            let (physical, repetition, converted, logical) = match column.kind {
                Kind::Text => (6, 0, Some(0), Some(1)),
                Kind::OptionalText => (6, 1, Some(0), Some(1)),
                Kind::Date => (1, 0, Some(6), Some(6)),
                Kind::Int32 => (1, 0, None, None),
                Kind::Int64 => (2, 0, None, None),
            };
            meta.begin(0);
            meta.i32(1, physical);
            meta.i32(3, repetition);
            meta.text(4, column.name);
            if let Some(converted) = converted {
                meta.i32(6, converted);
            }
            if let Some(logical) = logical {
                meta.begin(10);
                meta.begin(logical);
                meta.end();
                meta.end();
            }
            meta.end();
        }
        meta.i64(3, self.groups.iter().map(|group| group.rows as i64).sum());
        meta.list(4, 12, self.groups.len());
        for group in &self.groups {
            meta.begin(0);
            meta.list(1, 12, group.chunks.len());
            for (chunk, column) in group.chunks.iter().zip(&self.columns) {
                let physical = match column.kind {
                    Kind::Text | Kind::OptionalText => 6,
                    Kind::Date | Kind::Int32 => 1,
                    Kind::Int64 => 2,
                };
                meta.begin(0);
                meta.i64(2, chunk.offset as i64);
                meta.begin(3);
                meta.i32(1, physical);
                if column.kind == Kind::OptionalText {
                    meta.list(2, 5, 2);
                    meta.zigzag(0);
                    meta.zigzag(3);
                } else {
                    meta.list(2, 5, 1);
                    meta.zigzag(0);
                }
                meta.list(3, 8, 1);
                meta.varint(column.name.len() as u64);
                meta.out.extend_from_slice(column.name.as_bytes());
                meta.i32(4, 0);
                meta.i64(5, group.rows as i64);
                meta.i64(6, chunk.size as i64);
                meta.i64(7, chunk.size as i64);
                meta.i64(9, chunk.offset as i64);
                meta.end();
                meta.end();
            }
            meta.i64(2, group.chunks.iter().map(|chunk| chunk.size as i64).sum());
            meta.i64(3, group.rows as i64);
            meta.end();
        }
        meta.text(6, "shopping-sessions");
        meta.out.push(0);
        let length = meta.out.len() as u32;
        meta.out.extend_from_slice(&length.to_le_bytes());
        meta.out.extend_from_slice(b"PAR1");
        self.file.write_all(&meta.out)
    }
}
