use parquet::{
    basic::Encoding,
    data_type::{ByteArray, ByteArrayType, Int32Type, Int64Type},
    file::{properties::WriterProperties, writer::SerializedFileWriter},
    schema::parser::parse_message_type,
};
use std::{
    fs::{self, File},
    io,
    path::Path,
    sync::Arc,
};
pub enum Column {
    Text(Vec<ByteArray>),
    Optional(Vec<ByteArray>, Vec<i16>),
    Day(Vec<i32>),
    Count(Vec<i64>),
}
pub struct Report {
    writer: SerializedFileWriter<File>,
    columns: Vec<Column>,
    rows: usize,
}
impl Report {
    pub fn new(root: &Path, name: &str, fields: &[(&str, &str)]) -> io::Result<Self> {
        Self::part(root, name, 0, fields)
    }
    pub fn part(root: &Path, name: &str, part: usize, fields: &[(&str, &str)]) -> io::Result<Self> {
        let dir = root.join(name);
        fs::create_dir_all(&dir)?;
        let mut schema = String::from("message report {");
        let mut columns = Vec::new();
        for &(name, kind) in fields {
            let (decl, col) = match kind {
                "text" => (
                    format!("required binary {name} (UTF8);"),
                    Column::Text(Vec::new()),
                ),
                "optional" => (
                    format!("optional binary {name} (UTF8);"),
                    Column::Optional(Vec::new(), Vec::new()),
                ),
                "day" => (
                    format!("required int32 {name} (DATE);"),
                    Column::Day(Vec::new()),
                ),
                _ => (format!("required int64 {name};"), Column::Count(Vec::new())),
            };
            schema.push_str(&decl);
            columns.push(col);
        }
        schema.push('}');
        let props = WriterProperties::builder()
            .set_dictionary_enabled(false)
            .set_encoding(Encoding::PLAIN)
            .set_statistics_enabled(parquet::file::properties::EnabledStatistics::None)
            .build();
        let writer = SerializedFileWriter::new(
            File::create(dir.join(format!("part-{part}.parquet")))?,
            Arc::new(parse_message_type(&schema).map_err(io::Error::other)?),
            Arc::new(props),
        )
        .map_err(io::Error::other)?;
        Ok(Self {
            writer,
            columns,
            rows: 0,
        })
    }
    pub fn text(&mut self, i: usize, value: &[u8]) {
        let Column::Text(v) = &mut self.columns[i] else {
            unreachable!()
        };
        v.push(ByteArray::from(value.to_vec()));
    }
    pub fn day(&mut self, i: usize, value: i64) {
        let Column::Day(v) = &mut self.columns[i] else {
            unreachable!()
        };
        v.push(value as i32);
    }
    pub fn count(&mut self, i: usize, value: i64) {
        let Column::Count(v) = &mut self.columns[i] else {
            unreachable!()
        };
        v.push(value);
    }
    pub fn optional(&mut self, i: usize, value: Option<&[u8]>) {
        let Column::Optional(v, defs) = &mut self.columns[i] else {
            unreachable!()
        };
        defs.push(i16::from(value.is_some()));
        if let Some(s) = value {
            v.push(ByteArray::from(s.to_vec()));
        }
    }
    pub fn row(&mut self) -> io::Result<()> {
        self.rows += 1;
        if self.rows >= 65536 {
            self.flush()?;
        }
        Ok(())
    }
    fn flush(&mut self) -> io::Result<()> {
        if self.rows == 0 {
            return Ok(());
        }
        let mut group = self.writer.next_row_group().map_err(io::Error::other)?;
        for values in &mut self.columns {
            let mut column = group.next_column().map_err(io::Error::other)?.unwrap();
            match values {
                Column::Text(v) => {
                    column
                        .typed::<ByteArrayType>()
                        .write_batch(v, None, None)
                        .map_err(io::Error::other)?;
                    v.clear();
                }
                Column::Optional(v, d) => {
                    column
                        .typed::<ByteArrayType>()
                        .write_batch(v, Some(d), None)
                        .map_err(io::Error::other)?;
                    v.clear();
                    d.clear();
                }
                Column::Day(v) => {
                    column
                        .typed::<Int32Type>()
                        .write_batch(v, None, None)
                        .map_err(io::Error::other)?;
                    v.clear();
                }
                Column::Count(v) => {
                    column
                        .typed::<Int64Type>()
                        .write_batch(v, None, None)
                        .map_err(io::Error::other)?;
                    v.clear();
                }
            }
            column.close().map_err(io::Error::other)?;
        }
        group.close().map_err(io::Error::other)?;
        self.rows = 0;
        Ok(())
    }
    pub fn close(mut self) -> io::Result<()> {
        self.flush()?;
        self.writer.close().map_err(io::Error::other)?;
        Ok(())
    }
}
