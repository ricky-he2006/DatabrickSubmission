use parquet::{
    basic::Encoding,
    data_type::{ByteArray, ByteArrayType, Int32Type, Int64Type},
    file::{properties::WriterProperties, writer::SerializedFileWriter},
    schema::parser::parse_message_type,
};
use std::{fs::{self, File}, io::{self, BufWriter}, path::Path, sync::Arc};

const ROW_GROUP: usize = 1_048_576;

pub enum Column {
    Text(Vec<ByteArray>),
    Optional(Vec<ByteArray>, Vec<i16>),
    Day(Vec<i32>),
    Count(Vec<i64>),
}

pub struct Report {
    writer: SerializedFileWriter<BufWriter<File>>,
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
        let mut columns = Vec::with_capacity(fields.len());
        for &(name, kind) in fields {
            let (decl, col) = match kind {
                "text" => (format!("required binary {name} (UTF8);"), Column::Text(Vec::with_capacity(ROW_GROUP))),
                "optional" => (format!("optional binary {name} (UTF8);"), Column::Optional(Vec::with_capacity(ROW_GROUP), Vec::with_capacity(ROW_GROUP))),
                "day" => (format!("required int32 {name} (DATE);"), Column::Day(Vec::with_capacity(ROW_GROUP))),
                _ => (format!("required int64 {name};"), Column::Count(Vec::with_capacity(ROW_GROUP))),
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
        let file = File::create(dir.join(format!("part-{part}.parquet")))?;
        let writer = SerializedFileWriter::new(
            BufWriter::with_capacity(4 * 1024 * 1024, file),
            Arc::new(parse_message_type(&schema).map_err(io::Error::other)?),
            Arc::new(props),
        ).map_err(io::Error::other)?;
        Ok(Self { writer, columns, rows: 0 })
    }

    pub fn text(&mut self, i: usize, v: &[u8]) { if let Column::Text(c) = &mut self.columns[i] { c.push(ByteArray::from(v.to_vec())); } }
    pub fn day(&mut self, i: usize, v: i64) { if let Column::Day(c) = &mut self.columns[i] { c.push(v as i32); } }
    pub fn count(&mut self, i: usize, v: i64) { if let Column::Count(c) = &mut self.columns[i] { c.push(v); } }
    pub fn optional(&mut self, i: usize, v: Option<&[u8]>) {
        if let Column::Optional(c, d) = &mut self.columns[i] {
            d.push(i16::from(v.is_some()));
            if let Some(s) = v { c.push(ByteArray::from(s.to_vec())); }
        }
    }

    pub fn row(&mut self) -> io::Result<()> {
        self.rows += 1;
        if self.rows >= ROW_GROUP { self.flush()?; }
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.rows == 0 { return Ok(()); }
        let mut group = self.writer.next_row_group().map_err(io::Error::other)?;
        for values in &mut self.columns {
            let mut column = group.next_column().map_err(io::Error::other)?.unwrap();
            match values {
                Column::Text(v) => { column.typed::<ByteArrayType>().write_batch(v, None, None).map_err(io::Error::other)?; v.clear(); }
                Column::Optional(v, d) => { column.typed::<ByteArrayType>().write_batch(v, Some(&d[..]), None).map_err(io::Error::other)?; v.clear(); d.clear(); }
                Column::Day(v) => { column.typed::<Int32Type>().write_batch(v, None, None).map_err(io::Error::other)?; v.clear(); }
                Column::Count(v) => { column.typed::<Int64Type>().write_batch(v, None, None).map_err(io::Error::other)?; v.clear(); }
            }
            column.close().map_err(io::Error::other)?;
        }
        group.close().map_err(io::Error::other)?;
        self.writer.flush()?;
        self.rows = 0;
        Ok(())
    }

    pub fn finish(mut self) -> io::Result<()> {
        self.flush()?;
        self.writer.close().map_err(io::Error::other)?;
        Ok(())
    }
}
