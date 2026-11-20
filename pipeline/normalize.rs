use std::{
    env,
    fs::{self, File},
    io::{self, BufRead, BufReader, BufWriter, Write},
    path::PathBuf,
};
fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().collect();
    let width: usize = args[3].parse().unwrap();
    let mut paths: Vec<PathBuf> = fs::read_dir(&args[1])?
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "csv"))
        .collect();
    paths.sort();
    let mut out = BufWriter::with_capacity(1024 * 1024, File::create(&args[2])?);
    writeln!(out, "{}", args[4])?;
    let mut line = Vec::new();
    for path in paths {
        let mut input = BufReader::with_capacity(1024 * 1024, File::open(path)?);
        line.clear();
        input.read_until(b'\n', &mut line)?;
        loop {
            line.clear();
            if input.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            if line.last() == Some(&b'\n') {
                line.pop();
            }
            let mut fields: Vec<&[u8]> = Vec::with_capacity(width);
            let mut valid = true;
            for raw in line.split(|&b| b == b',') {
                let value = if raw.len() >= 2 && raw[0] == b'"' && raw[raw.len() - 1] == b'"' {
                    &raw[1..raw.len() - 1]
                } else {
                    raw
                };
                if value.contains(&b'"') {
                    valid = false;
                    break;
                }
                fields.push(value);
            }
            if !valid || fields.len() != width {
                continue;
            }
            for (i, value) in fields.iter().enumerate() {
                if i > 0 {
                    out.write_all(b",")?;
                }
                let quote = value.contains(&b'\r');
                if quote {
                    out.write_all(b"\"")?;
                }
                out.write_all(value)?;
                if quote {
                    out.write_all(b"\"")?;
                }
            }
            out.write_all(b"\n")?;
        }
    }
    out.flush()
}
