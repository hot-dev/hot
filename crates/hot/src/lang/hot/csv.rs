// CSV and TSV functions for ::hot::csv.
//
// `from-csv` and `iter-csv` read through one `RowReader`: the first collects
// what the second yields one row at a time. Error messages name lines, rows and
// columns but never hold a cell, since errors travel to run records and alerts
// and rows often hold personal data.

use crate::lang::hot::iter::{HotIterator, IteratorBox, VecIterator};
use crate::lang::hot::r#type::{HotResult, untype_recursive};
use crate::lang::runtime::limits;
use crate::val::{Val, tagged_type_name};
use crate::validate_args_range;
use ahash::AHashSet;
use fastnum::D256;
use indexmap::{IndexMap, IndexSet};
use std::borrow::Cow;
use std::collections::VecDeque;
use std::io::{self, Read};
use std::sync::{Arc, Mutex};

const COMMA: u8 = b',';
const TAB: u8 = b'\t';
const UTF8_BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

/// The non-null options in `opts`, refusing any name not in `allowed`, so that
/// a misspelled option can't silently parse a file the wrong way.
pub(crate) fn option_entries<'a>(
    opts: Option<&'a Val>,
    allowed: &[&str],
) -> Result<Vec<(&'a str, &'a Val)>, String> {
    let Some(opts) = opts else {
        return Ok(Vec::new());
    };
    let map = match opts {
        Val::Null => return Ok(Vec::new()),
        Val::Map(map) => map,
        _ => return Err("options must be a Map".to_string()),
    };
    let mut entries = Vec::with_capacity(map.len());
    for (key, value) in map.iter() {
        let Val::Str(name) = key else {
            return Err("option names must be Str".to_string());
        };
        if !allowed.contains(&&**name) {
            return Err(format!("unknown option \"{name}\""));
        }
        if !matches!(value, Val::Null) {
            entries.push((&**name, value));
        }
    }
    Ok(entries)
}

fn bool_option(name: &str, value: &Val) -> Result<bool, String> {
    match value {
        Val::Bool(flag) => Ok(*flag),
        _ => Err(format!("option \"{name}\" must be a Bool")),
    }
}

/// A delimiter, quote or comment character: the `csv` crate works in bytes.
fn char_option(name: &str, value: &Val) -> Result<u8, String> {
    match value {
        Val::Str(text) if text.len() == 1 && text.is_ascii() && !matches!(&**text, "\r" | "\n") => {
            Ok(text.as_bytes()[0])
        }
        _ => Err(format!(
            "option \"{name}\" must be a single ASCII character other than a line ending"
        )),
    }
}

fn columns_option(value: &Val) -> Result<Vec<Arc<str>>, String> {
    let Val::Vec(items) = value else {
        return Err("option \"columns\" must be a Vec of Str".to_string());
    };
    let mut names = Vec::with_capacity(items.len());
    for item in items.iter() {
        let Val::Str(name) = item else {
            return Err("option \"columns\" must be a Vec of Str".to_string());
        };
        names.push(Arc::clone(name));
    }
    check_names(names.iter().map(|name| &**name))?;
    if names.is_empty() {
        return Err("option \"columns\" must name at least one column".to_string());
    }
    Ok(names)
}

/// Column names become Map keys, so they must be distinct and present. `$type`
/// would make each row a value of whatever type the file names.
fn check_names<'a>(names: impl Iterator<Item = &'a str>) -> Result<(), String> {
    let mut seen = AHashSet::new();
    for (index, name) in names.enumerate() {
        if name.trim().is_empty() {
            return Err(format!(
                "column {} has a blank name; pass \"columns\" to name them",
                index + 1
            ));
        }
        if name == "$type" {
            return Err(format!(
                "column {} can't be named \"$type\"; pass \"columns\" to name them",
                index + 1
            ));
        }
        if !seen.insert(name) {
            return Err(format!(
                "column name \"{name}\" appears twice; pass \"columns\" to name them"
            ));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum CellType {
    Int,
    Dec,
    Bool,
}

impl CellType {
    fn name(self) -> &'static str {
        match self {
            CellType::Int => "Int",
            CellType::Dec => "Dec",
            CellType::Bool => "Bool",
        }
    }

    /// The cell as this type. An empty cell is null, which is what `to-csv`
    /// writes null as.
    fn convert(self, cell: &str) -> Option<Val> {
        if cell.is_empty() {
            return Some(Val::Null);
        }
        match self {
            CellType::Int => cell.parse::<i64>().ok().map(Val::Int),
            CellType::Dec => cell
                .parse::<D256>()
                .ok()
                .filter(|number| number.is_finite())
                .map(Val::Dec),
            CellType::Bool => {
                if cell.eq_ignore_ascii_case("true") {
                    Some(Val::Bool(true))
                } else if cell.eq_ignore_ascii_case("false") {
                    Some(Val::Bool(false))
                } else {
                    None
                }
            }
        }
    }
}

fn types_option(value: &Val) -> Result<Vec<(Arc<str>, CellType)>, String> {
    let Val::Map(map) = value else {
        return Err("option \"types\" must be a Map of column name to type".to_string());
    };
    let mut types = Vec::with_capacity(map.len());
    for (column, type_name) in map.iter() {
        let Val::Str(column) = column else {
            return Err("option \"types\" must be keyed by column name".to_string());
        };
        let cell_type = match type_name {
            Val::Str(name) if &**name == "Int" => CellType::Int,
            Val::Str(name) if &**name == "Dec" => CellType::Dec,
            Val::Str(name) if &**name == "Bool" => CellType::Bool,
            _ => {
                return Err(format!(
                    "option \"types\": column \"{column}\" must be \"Int\", \"Dec\" or \"Bool\""
                ));
            }
        };
        types.push((Arc::clone(column), cell_type));
    }
    Ok(types)
}

#[derive(Debug)]
struct ReadOpts {
    delimiter: u8,
    /// `None` is `quote: false`: a quote character is ordinary text.
    quote: Option<u8>,
    comment: Option<u8>,
    trim: bool,
    skip_empty: bool,
    strict: bool,
    has_header: bool,
    columns: Option<Vec<Arc<str>>>,
    types: Vec<(Arc<str>, CellType)>,
}

impl ReadOpts {
    fn parse(opts: Option<&Val>, default_delimiter: u8) -> Result<Self, String> {
        let mut parsed = ReadOpts {
            delimiter: default_delimiter,
            quote: Some(b'"'),
            comment: None,
            trim: false,
            skip_empty: false,
            strict: true,
            has_header: true,
            columns: None,
            types: Vec::new(),
        };
        let mut header = None;
        for (name, value) in option_entries(
            opts,
            &[
                "delimiter",
                "quote",
                "header",
                "columns",
                "comment",
                "trim",
                "skip-empty",
                "strict",
                "types",
            ],
        )? {
            match name {
                "delimiter" => parsed.delimiter = char_option(name, value)?,
                "quote" => {
                    parsed.quote = match value {
                        Val::Bool(false) => None,
                        _ => Some(char_option(name, value)?),
                    }
                }
                "header" => header = Some(bool_option(name, value)?),
                "columns" => parsed.columns = Some(columns_option(value)?),
                "comment" => parsed.comment = Some(char_option(name, value)?),
                "trim" => parsed.trim = bool_option(name, value)?,
                "skip-empty" => parsed.skip_empty = bool_option(name, value)?,
                "strict" => parsed.strict = bool_option(name, value)?,
                _ => parsed.types = types_option(value)?,
            }
        }
        // `columns` names data that has no header, so a first row is data unless
        // told otherwise: guessing wrong shows a header as a row, where the
        // opposite guess would lose a row.
        parsed.has_header = header.unwrap_or(parsed.columns.is_none());
        if parsed.quote == Some(parsed.delimiter) {
            return Err("options \"delimiter\" and \"quote\" must differ".to_string());
        }
        if let Some(comment) = parsed.comment
            && (comment == parsed.delimiter || Some(comment) == parsed.quote)
        {
            return Err("option \"comment\" must differ from \"delimiter\" and \"quote\"".into());
        }
        if !parsed.types.is_empty() && !parsed.has_header && parsed.columns.is_none() {
            return Err("option \"types\" needs column names: a header or \"columns\"".to_string());
        }
        Ok(parsed)
    }
}

// ---------------------------------------------------------------------------
// Input checks
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
enum ScanState {
    RecordStart,
    FieldStart,
    Unquoted,
    /// In a quoted field, opened on this line.
    Quoted(u64),
    AfterQuote(u64),
    Comment,
}

/// Input on its way to the `csv` crate, checked for what that crate reads
/// without complaint. It takes an unclosed quote to run to the end of the file,
/// which would turn the rest of a file into one cell, and reads `"a"b` as `ab`.
///
/// This also strips a BOM and keeps a row from growing without limit, which a
/// stream could otherwise do by never closing a quote.
#[derive(Debug)]
struct CheckedInput<R> {
    inner: R,
    delimiter: u8,
    quote: Option<u8>,
    comment: Option<u8>,
    state: ScanState,
    /// Up to the first three bytes, read to look for a BOM.
    head: Option<([u8; 3], usize, usize)>,
    line: u64,
    after_cr: bool,
    /// The line each record read so far but not yet taken by `RowReader` starts
    /// on. `csv` reports where the last record ended, which is a line short after
    /// CRLF, a comment or a blank line, and it doesn't count a lone `\r`.
    record_lines: VecDeque<u64>,
    row_bytes: usize,
}

impl<R: Read> CheckedInput<R> {
    fn new(inner: R, opts: &ReadOpts) -> Self {
        CheckedInput {
            inner,
            delimiter: opts.delimiter,
            quote: opts.quote,
            comment: opts.comment,
            state: ScanState::RecordStart,
            head: None,
            line: 1,
            after_cr: false,
            record_lines: VecDeque::new(),
            row_bytes: 0,
        }
    }

    fn invalid(message: String) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, message)
    }

    fn scan(&mut self, chunk: &[u8]) -> io::Result<()> {
        for &byte in chunk {
            if self.after_cr && byte != b'\n' {
                self.line += 1;
            }
            self.after_cr = byte == b'\r';
            let ends_line = byte == b'\n' || byte == b'\r';
            let is_quote = Some(byte) == self.quote;
            let starts_field = |line| {
                if is_quote {
                    ScanState::Quoted(line)
                } else if byte == self.delimiter {
                    ScanState::FieldStart
                } else if ends_line {
                    ScanState::RecordStart
                } else {
                    ScanState::Unquoted
                }
            };
            let previous = self.state;
            self.state = match previous {
                ScanState::RecordStart if ends_line => ScanState::RecordStart,
                ScanState::RecordStart if Some(byte) == self.comment => ScanState::Comment,
                ScanState::RecordStart | ScanState::FieldStart => starts_field(self.line),
                ScanState::Unquoted if is_quote => {
                    return Err(Self::invalid(format!(
                        "line {}: quote inside an unquoted field; quote the whole field and \
                         double the quote, or pass quote: false if quotes are ordinary text",
                        self.line
                    )));
                }
                ScanState::Unquoted => {
                    if byte == self.delimiter {
                        ScanState::FieldStart
                    } else if ends_line {
                        ScanState::RecordStart
                    } else {
                        ScanState::Unquoted
                    }
                }
                ScanState::Quoted(opened) if is_quote => ScanState::AfterQuote(opened),
                ScanState::Quoted(opened) => ScanState::Quoted(opened),
                ScanState::AfterQuote(opened) if is_quote => ScanState::Quoted(opened),
                ScanState::AfterQuote(_) if byte == self.delimiter => ScanState::FieldStart,
                ScanState::AfterQuote(_) if ends_line => ScanState::RecordStart,
                ScanState::AfterQuote(_) => {
                    return Err(Self::invalid(format!(
                        "line {}: text after a closing quote",
                        self.line
                    )));
                }
                ScanState::Comment if byte == b'\n' => ScanState::RecordStart,
                ScanState::Comment => ScanState::Comment,
            };
            match (previous, self.state) {
                (_, ScanState::RecordStart | ScanState::Comment) => self.row_bytes = 0,
                (ScanState::RecordStart, _) => {
                    self.record_lines.push_back(self.line);
                    self.row_bytes = 1;
                }
                _ => self.row_bytes += 1,
            }
            if byte == b'\n' {
                self.line += 1;
            }
        }
        let cap = limits::max_parse_input_bytes();
        if self.row_bytes > cap {
            return Err(Self::invalid(format!(
                "line {}: refusing to read a row of more than {cap} bytes. Raise \
                 HOT_MAX_PARSE_INPUT_BYTES if this is intentional",
                self.line
            )));
        }
        Ok(())
    }

    fn fill_head(&mut self) -> io::Result<()> {
        let mut head = [0u8; 3];
        let mut filled = 0;
        while filled < head.len() {
            match self.inner.read(&mut head[filled..])? {
                0 => break,
                read => filled += read,
            }
        }
        let start = if head == UTF8_BOM { filled } else { 0 };
        self.head = Some((head, start, filled));
        Ok(())
    }
}

impl<R: Read> Read for CheckedInput<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.head.is_none() {
            self.fill_head()?;
        }
        let read = match &mut self.head {
            Some((head, start, filled)) if *start < *filled => {
                let read = (*filled - *start).min(buf.len());
                buf[..read].copy_from_slice(&head[*start..*start + read]);
                *start += read;
                read
            }
            _ => self.inner.read(buf)?,
        };
        if read == 0 {
            if let ScanState::Quoted(opened) = self.state {
                return Err(Self::invalid(format!(
                    "line {opened}: quoted field is never closed"
                )));
            }
            return Ok(0);
        }
        self.scan(&buf[..read])?;
        Ok(read)
    }
}

// ---------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------

/// What a row becomes once its column names are known.
#[derive(Debug)]
enum RowShape {
    /// The first row will name the columns.
    FromHeader,
    Named {
        /// The column names with null values, cloned for each row so that a
        /// row needn't hash its keys again.
        template: IndexMap<Val, Val>,
        types: Vec<Option<CellType>>,
    },
    /// No names: rows are Vecs, as wide as the first one.
    Plain { width: Option<usize> },
}

fn row_shape(names: &[Arc<str>], types: &[(Arc<str>, CellType)]) -> Result<RowShape, String> {
    let template = names
        .iter()
        .map(|name| (Val::Str(Arc::clone(name)), Val::Null))
        .collect();
    let mut column_types = vec![None; names.len()];
    for (column, cell_type) in types {
        let Some(index) = names.iter().position(|name| name == column) else {
            return Err(format!(
                "option \"types\" names column \"{column}\", which the data doesn't have"
            ));
        };
        column_types[index] = Some(*cell_type);
    }
    Ok(RowShape::Named {
        template,
        types: column_types,
    })
}

#[derive(Debug)]
struct RowReader<R> {
    reader: csv::Reader<CheckedInput<R>>,
    record: csv::StringRecord,
    shape: RowShape,
    skip_header: bool,
    strict: bool,
    skip_empty: bool,
    types: Vec<(Arc<str>, CellType)>,
    /// The line the current record starts on.
    line: u64,
    row: u64,
    empty: Val,
}

impl<R: Read> RowReader<R> {
    fn new(input: R, opts: ReadOpts) -> Result<Self, String> {
        let checked = CheckedInput::new(input, &opts);
        let reader = csv::ReaderBuilder::new()
            .has_headers(false)
            .flexible(true)
            .delimiter(opts.delimiter)
            .quote(opts.quote.unwrap_or(b'"'))
            .quoting(opts.quote.is_some())
            .comment(opts.comment)
            .trim(if opts.trim {
                csv::Trim::All
            } else {
                csv::Trim::None
            })
            .from_reader(checked);
        let (shape, skip_header) = match &opts.columns {
            Some(names) => (row_shape(names, &opts.types)?, opts.has_header),
            None if opts.has_header => (RowShape::FromHeader, false),
            None => (RowShape::Plain { width: None }, false),
        };
        Ok(RowReader {
            reader,
            record: csv::StringRecord::new(),
            shape,
            skip_header,
            strict: opts.strict,
            skip_empty: opts.skip_empty,
            types: opts.types,
            line: 0,
            row: 0,
            empty: Val::from(""),
        })
    }

    fn describe(&self, error: csv::Error) -> String {
        match error.kind() {
            csv::ErrorKind::Io(cause) => cause.to_string(),
            csv::ErrorKind::Utf8 { .. } => {
                format!("line {}: invalid UTF-8; CSV input must be UTF-8", self.line)
            }
            _ => error.to_string(),
        }
    }

    fn cell(&self, cell: &str) -> Val {
        if cell.is_empty() {
            self.empty.clone()
        } else {
            Val::from(cell)
        }
    }

    fn check_width(&self, width: usize) -> Result<(), String> {
        if self.strict && self.record.len() != width {
            return Err(format!(
                "line {}: expected {width} fields, found {}",
                self.line,
                self.record.len()
            ));
        }
        Ok(())
    }

    /// The next row, or `None` at the end of the input.
    fn next_row(&mut self) -> Result<Option<Val>, String> {
        loop {
            let read = self.reader.read_record(&mut self.record);
            let line = self.reader.get_mut().record_lines.pop_front();
            match (read, line) {
                (Ok(false), _) => return Ok(None),
                // A last comment with no newline makes `csv` add an empty record,
                // which isn't in the input.
                (Ok(true), None) => continue,
                (Ok(true), Some(line)) => self.line = line,
                (Err(error), line) => {
                    self.line = line.unwrap_or(self.line);
                    return Err(self.describe(error));
                }
            }
            if self.skip_empty && self.record.iter().all(str::is_empty) {
                continue;
            }
            if self.skip_header {
                self.skip_header = false;
                continue;
            }
            if let RowShape::FromHeader = self.shape {
                let names: Vec<Arc<str>> = self.record.iter().map(Arc::from).collect();
                check_names(names.iter().map(|name| &**name))
                    .map_err(|error| format!("line {}: {error}", self.line))?;
                self.shape = row_shape(&names, &self.types)?;
                continue;
            }
            limits::check_collection_size("csv row", self.record.len())
                .map_err(|error| error_text(&error))?;
            self.row += 1;
            return self.build_row().map(Some);
        }
    }

    fn build_row(&mut self) -> Result<Val, String> {
        match &self.shape {
            RowShape::FromHeader => Err("no column names were read".to_string()),
            RowShape::Plain { width } => {
                let width = width.unwrap_or(self.record.len());
                self.check_width(width)?;
                let mut cells: Vec<Val> = self.record.iter().map(|cell| self.cell(cell)).collect();
                if cells.len() < width {
                    cells.resize(width, Val::Null);
                }
                self.shape = RowShape::Plain { width: Some(width) };
                Ok(Val::vec(cells))
            }
            RowShape::Named { template, types } => {
                self.check_width(template.len())?;
                let mut row = template.clone();
                for (index, ((name, slot), cell)) in
                    row.iter_mut().zip(self.record.iter()).enumerate()
                {
                    *slot = match types[index] {
                        None => self.cell(cell),
                        Some(cell_type) => cell_type.convert(cell).ok_or_else(|| {
                            format!(
                                "line {}, row {}, column {}: not a valid {}",
                                self.line,
                                self.row,
                                column_label(name),
                                cell_type.name()
                            )
                        })?,
                    };
                }
                Ok(Val::map(row))
            }
        }
    }
}

fn column_label(name: &Val) -> String {
    match name {
        Val::Str(text) => format!("\"{text}\""),
        other => other.to_string(),
    }
}

/// The text of an error from `limits`, which are Hot strings.
fn error_text(error: &Val) -> String {
    match error {
        Val::Str(text) => text.to_string(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// from-csv, from-tsv
// ---------------------------------------------------------------------------

/// Parse all of `args[0]`, rejecting all of it if any of it is malformed.
fn read_all(op: &str, args: &[Val], default_delimiter: u8) -> HotResult<Val> {
    let fail = |message: String| HotResult::Err(Val::from(format!("{op}: {message}")));
    let text: &[u8] = match &args[0] {
        Val::Str(text) => text.as_bytes(),
        Val::Bytes(bytes) => bytes,
        _ => return fail("expects Str or Bytes".to_string()),
    };
    if let Err(error) = limits::check_parse_input(op, text.len()) {
        return HotResult::Err(error);
    }
    let opts = match ReadOpts::parse(args.get(1), default_delimiter) {
        Ok(opts) => opts,
        Err(error) => return fail(error),
    };
    let mut reader = match RowReader::new(text, opts) {
        Ok(reader) => reader,
        Err(error) => return fail(error),
    };
    let mut rows = Vec::new();
    let mut cells = 0;
    loop {
        match reader.next_row() {
            Ok(Some(row)) => {
                // Cells, not just rows: a million short rows cost what a
                // million-element Vec costs several times over.
                cells += reader.record.len().max(1);
                if let Err(error) = limits::check_collection_size(op, cells) {
                    return HotResult::Err(error);
                }
                rows.push(row);
            }
            Ok(None) => return HotResult::Ok(Val::vec(rows)),
            Err(error) => return fail(error),
        }
    }
}

pub fn from_csv(args: &[Val]) -> HotResult<Val> {
    validate_args_range!("::hot::csv/from-csv", args, 1, 2);
    read_all("from-csv", args, COMMA)
}

pub fn from_tsv(args: &[Val]) -> HotResult<Val> {
    validate_args_range!("::hot::csv/from-tsv", args, 1, 2);
    read_all("from-tsv", args, TAB)
}

// ---------------------------------------------------------------------------
// iter-csv, iter-tsv
// ---------------------------------------------------------------------------

/// The chunks an iterator yields, as a byte stream. Calling the source's own
/// `next` keeps its chunks out of `stream:data`, which is for values a program
/// reads, and out of call records.
#[derive(Debug)]
pub(crate) struct ChunkInput {
    source: Arc<Mutex<Box<dyn HotIterator>>>,
    chunk: Val,
    position: usize,
    done: bool,
}

impl ChunkInput {
    pub(crate) fn new(source: Arc<Mutex<Box<dyn HotIterator>>>) -> Self {
        ChunkInput {
            source,
            chunk: Val::Null,
            position: 0,
            done: false,
        }
    }

    fn pull(&mut self) -> io::Result<()> {
        let mut source = self
            .source
            .lock()
            .map_err(|_| io::Error::other("source iterator failed earlier"))?;
        let (chunk, done) = source.next().map_err(io::Error::other)?;
        drop(source);
        match chunk {
            _ if done => self.done = true,
            Val::Null => {}
            Val::Str(_) | Val::Bytes(_) => {
                self.chunk = chunk;
                self.position = 0;
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "source iterator must yield Str or Bytes chunks",
                ));
            }
        }
        Ok(())
    }
}

impl Read for ChunkInput {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let chunk: &[u8] = match &self.chunk {
                Val::Str(text) => text.as_bytes(),
                Val::Bytes(bytes) => bytes,
                _ => &[],
            };
            if self.position < chunk.len() {
                let read = (chunk.len() - self.position).min(buf.len());
                buf[..read].copy_from_slice(&chunk[self.position..self.position + read]);
                self.position += read;
                return Ok(read);
            }
            if self.done {
                return Ok(0);
            }
            self.chunk = Val::Null;
            self.position = 0;
            self.pull()?;
        }
    }
}

/// Rows, read as they're asked for.
#[derive(Debug)]
struct RowIterator {
    op: &'static str,
    rows: RowReader<ChunkInput>,
    /// An error ends the rows for good. Later calls report it again, so nothing
    /// mistakes it for the end of the file.
    failed: Option<String>,
}

impl HotIterator for RowIterator {
    fn next(&mut self) -> Result<(Val, bool), String> {
        if let Some(error) = &self.failed {
            return Err(error.clone());
        }
        match self.rows.next_row() {
            Ok(Some(row)) => Ok((row, false)),
            Ok(None) => Ok((Val::Null, true)),
            Err(error) => {
                let error = format!("{}: {error}", self.op);
                self.failed = Some(error.clone());
                Err(error)
            }
        }
    }
}

fn read_lazily(op: &'static str, args: &[Val], default_delimiter: u8) -> HotResult<Val> {
    let fail = |message: String| HotResult::Err(Val::from(format!("{op}: {message}")));
    let source = match &args[0] {
        Val::Box(boxed) => match boxed.as_any().downcast_ref::<IteratorBox>() {
            Some(iterator) => Arc::clone(&iterator.inner),
            None => return fail("expects an Iter, Str or Bytes".to_string()),
        },
        chunk @ (Val::Str(_) | Val::Bytes(_)) => {
            let only: Box<dyn HotIterator> = Box::new(VecIterator::new(vec![chunk.clone()]));
            Arc::new(Mutex::new(only))
        }
        _ => return fail("expects an Iter, Str or Bytes".to_string()),
    };
    let opts = match ReadOpts::parse(args.get(1), default_delimiter) {
        Ok(opts) => opts,
        Err(error) => return fail(error),
    };
    let rows = match RowReader::new(ChunkInput::new(source), opts) {
        Ok(rows) => rows,
        Err(error) => return fail(error),
    };
    let iterator = RowIterator {
        op,
        rows,
        failed: None,
    };
    HotResult::Ok(Val::Box(Box::new(IteratorBox::new(Box::new(iterator)))))
}

pub fn iter_csv(args: &[Val]) -> HotResult<Val> {
    validate_args_range!("::hot::csv/iter-csv", args, 1, 2);
    read_lazily("iter-csv", args, COMMA)
}

pub fn iter_tsv(args: &[Val]) -> HotResult<Val> {
    validate_args_range!("::hot::csv/iter-tsv", args, 1, 2);
    read_lazily("iter-tsv", args, TAB)
}

// ---------------------------------------------------------------------------
// to-csv, to-tsv
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct WriteOpts {
    delimiter: u8,
    quote: u8,
    columns: Option<Vec<Arc<str>>>,
    header: bool,
    crlf: bool,
    escape_formulas: bool,
}

impl WriteOpts {
    fn parse(opts: Option<&Val>, default_delimiter: u8) -> Result<Self, String> {
        let mut parsed = WriteOpts {
            delimiter: default_delimiter,
            quote: b'"',
            columns: None,
            header: true,
            crlf: false,
            escape_formulas: false,
        };
        for (name, value) in option_entries(
            opts,
            &[
                "delimiter",
                "quote",
                "columns",
                "header",
                "line-ending",
                "escape-formulas",
            ],
        )? {
            match name {
                "delimiter" => parsed.delimiter = char_option(name, value)?,
                "quote" => parsed.quote = char_option(name, value)?,
                "columns" => parsed.columns = Some(columns_option(value)?),
                "header" => parsed.header = bool_option(name, value)?,
                "line-ending" => {
                    parsed.crlf = match value {
                        Val::Str(ending) if &**ending == "\n" => false,
                        Val::Str(ending) if &**ending == "\r\n" => true,
                        _ => {
                            return Err(
                                "option \"line-ending\" must be \"\\n\" or \"\\r\\n\"".to_string()
                            );
                        }
                    }
                }
                _ => parsed.escape_formulas = bool_option(name, value)?,
            }
        }
        if parsed.quote == parsed.delimiter {
            return Err("options \"delimiter\" and \"quote\" must differ".to_string());
        }
        Ok(parsed)
    }
}

/// `text` as a spreadsheet will take as text, not as a formula to run.
fn escape_formula(text: &str) -> Cow<'_, str> {
    if text.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        Cow::Owned(format!("'{text}"))
    } else {
        Cow::Borrowed(text)
    }
}

/// A cell's text. Only a Str gets `escape-formulas`: a number can't hold a
/// formula, and `'-5` would no longer read as a number.
fn render_cell(cell: &Val, escape: bool) -> Result<Cow<'_, str>, &'static str> {
    match cell {
        Val::Null => Ok(Cow::Borrowed("")),
        Val::Str(text) if escape => Ok(escape_formula(text)),
        Val::Str(text) => Ok(Cow::Borrowed(text)),
        Val::Int(number) => Ok(Cow::Owned(number.to_string())),
        Val::Dec(number) => Ok(Cow::Owned(number.to_string())),
        Val::Bool(flag) => Ok(Cow::Borrowed(if *flag { "true" } else { "false" })),
        Val::Map(_) => Err("a Map"),
        Val::Vec(_) => Err("a Vec"),
        Val::Bytes(_) | Val::Byte(_) => Err("Bytes"),
        Val::Box(_) => Err("a value that has no text form"),
    }
}

/// A row as the Map or Vec it holds: a typed value's fields are its columns.
fn plain_row<'a>(row: &'a Val, number: usize) -> Result<Cow<'a, Val>, String> {
    match row {
        Val::Map(map) if tagged_type_name(map).is_some() => match untype_recursive(row) {
            HotResult::Ok(plain) => Ok(Cow::Owned(plain)),
            HotResult::Err(_) => Err(format!("row {number} can't be read as a Map")),
        },
        Val::Map(_) | Val::Vec(_) => Ok(Cow::Borrowed(row)),
        _ => Err(format!("row {number} must be a Map or a Vec")),
    }
}

/// The columns of Map rows, in the order they first appear.
fn columns_from_rows(rows: &[Val]) -> Result<Vec<Arc<str>>, String> {
    let mut names: IndexSet<Arc<str>> = IndexSet::new();
    for (index, row) in rows.iter().enumerate() {
        let row = plain_row(row, index + 1)?;
        if let Val::Map(map) = &*row {
            for key in map.keys() {
                let Val::Str(name) = key else {
                    return Err(format!("row {}: column names must be Str", index + 1));
                };
                if !names.contains(name) {
                    names.insert(Arc::clone(name));
                }
            }
        }
    }
    let names: Vec<Arc<str>> = names.into_iter().collect();
    check_names(names.iter().map(|name| &**name))?;
    Ok(names)
}

fn write_all(op: &str, args: &[Val], default_delimiter: u8) -> Result<String, String> {
    let Val::Vec(rows) = &args[0] else {
        return Err("expects a Vec of Maps or a Vec of Vecs".to_string());
    };
    let opts = WriteOpts::parse(args.get(1), default_delimiter)?;
    let map_rows = match rows.first().map(|row| plain_row(row, 1)).transpose()? {
        Some(first) => matches!(&*first, Val::Map(_)),
        None => false,
    };
    let columns = match &opts.columns {
        Some(names) => Some(names.clone()),
        None if map_rows => Some(columns_from_rows(rows)?),
        None => None,
    };
    let keys: Vec<Val> = columns
        .iter()
        .flatten()
        .map(|name| Val::Str(Arc::clone(name)))
        .collect();

    let mut writer = csv::WriterBuilder::new()
        .flexible(true)
        .delimiter(opts.delimiter)
        .quote(opts.quote)
        .terminator(if opts.crlf {
            csv::Terminator::CRLF
        } else {
            csv::Terminator::Any(b'\n')
        })
        .from_writer(Vec::new());
    let write_error = |error: csv::Error| error.to_string();

    if opts.header
        && let Some(names) = &columns
    {
        let record = names.iter().map(|name| {
            if opts.escape_formulas {
                escape_formula(name).into_owned()
            } else {
                name.to_string()
            }
        });
        writer.write_record(record).map_err(write_error)?;
    }

    let mut width = columns.as_ref().map(Vec::len);
    let mut cells: Vec<Cow<'_, str>> = Vec::new();
    let rows_plain: Vec<Cow<'_, Val>> = rows
        .iter()
        .enumerate()
        .map(|(index, row)| plain_row(row, index + 1))
        .collect::<Result<_, _>>()?;
    for (index, row) in rows_plain.iter().enumerate() {
        let number = index + 1;
        cells.clear();
        match &**row {
            Val::Map(map) if map_rows => {
                for (key, name) in keys.iter().zip(columns.iter().flatten()) {
                    let cell = map.get(key).unwrap_or(&Val::Null);
                    let text = render_cell(cell, opts.escape_formulas).map_err(|kind| {
                        format!("row {number}, column \"{name}\": can't write {kind} to a cell")
                    })?;
                    cells.push(text);
                }
            }
            Val::Vec(items) if !map_rows => {
                let expected = *width.get_or_insert(items.len());
                if items.len() != expected {
                    return Err(format!(
                        "row {number}: expected {expected} cells, found {}",
                        items.len()
                    ));
                }
                for (position, cell) in items.iter().enumerate() {
                    let text = render_cell(cell, opts.escape_formulas).map_err(|kind| {
                        let column = match columns.as_ref().and_then(|names| names.get(position)) {
                            Some(name) => format!("\"{name}\""),
                            None => (position + 1).to_string(),
                        };
                        format!("row {number}, column {column}: can't write {kind} to a cell")
                    })?;
                    cells.push(text);
                }
            }
            _ => {
                return Err(format!("row {number}: rows must be all Maps or all Vecs"));
            }
        }
        writer
            .write_record(cells.iter().map(|cell| cell.as_bytes()))
            .map_err(write_error)?;
        limits::check_string_bytes(op, writer.get_ref().len())
            .map_err(|error| error_text(&error))?;
    }
    let bytes = writer
        .into_inner()
        .map_err(|error| error.into_error().to_string())?;
    limits::check_string_bytes(op, bytes.len()).map_err(|error| error_text(&error))?;
    String::from_utf8(bytes).map_err(|error| error.to_string())
}

fn write_rows(op: &str, args: &[Val], default_delimiter: u8) -> HotResult<Val> {
    match write_all(op, args, default_delimiter) {
        Ok(text) => HotResult::Ok(Val::from(text)),
        Err(error) => HotResult::Err(Val::from(format!("{op}: {error}"))),
    }
}

pub fn to_csv(args: &[Val]) -> HotResult<Val> {
    validate_args_range!("::hot::csv/to-csv", args, 1, 2);
    write_rows("to-csv", args, COMMA)
}

pub fn to_tsv(args: &[Val]) -> HotResult<Val> {
    validate_args_range!("::hot::csv/to-tsv", args, 1, 2);
    write_rows("to-tsv", args, TAB)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(text: &[u8], opts: Val) -> Result<Vec<Val>, String> {
        let opts = ReadOpts::parse(Some(&opts), COMMA)?;
        let mut reader = RowReader::new(text, opts)?;
        let mut rows = Vec::new();
        while let Some(row) = reader.next_row()? {
            rows.push(row);
        }
        Ok(rows)
    }

    fn read_in_chunks(text: &[u8], opts: Val, size: usize) -> Result<Vec<Val>, String> {
        let chunks = text.chunks(size).map(|c| Val::bytes(c.to_vec())).collect();
        let source: Box<dyn HotIterator> = Box::new(VecIterator::new(chunks));
        let opts = ReadOpts::parse(Some(&opts), COMMA)?;
        let mut reader = RowReader::new(ChunkInput::new(Arc::new(Mutex::new(source))), opts)?;
        let mut rows = Vec::new();
        while let Some(row) = reader.next_row()? {
            rows.push(row);
        }
        Ok(rows)
    }

    fn plain() -> Val {
        crate::val!({"header": false, "strict": false, "comment": "#"})
    }

    /// Inputs built from the bytes that steer the parser, from a fixed sequence.
    fn awkward_inputs() -> impl Iterator<Item = Vec<u8>> {
        const ALPHABET: &[u8] = b"ab,\"\n\r#\xC3\xA9";
        let mut state: u64 = 0x2545_F491_4F6C_DD1D;
        (0..4000).map(move |_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let length = (state % 24) as usize;
            (0..length)
                .map(|step| {
                    let mixed = state.rotate_left((step * 7) as u32) ^ (step as u64 * 31);
                    ALPHABET[(mixed % ALPHABET.len() as u64) as usize]
                })
                .collect()
        })
    }

    #[test]
    fn chunk_boundaries_change_nothing() {
        for input in awkward_inputs() {
            // Which error a bad input reports first can differ. That it fails can't.
            let whole = read(&input, plain()).ok();
            for size in [1, 2, 3, 5] {
                assert_eq!(
                    whole,
                    read_in_chunks(&input, plain(), size).ok(),
                    "input {input:?}, chunks of {size}"
                );
            }
        }
    }

    /// The scanner has to find a record wherever the parser does, or errors
    /// would name the wrong lines and a row could be misjudged as malformed.
    #[test]
    fn scanner_finds_the_records_the_parser_does() {
        for input in awkward_inputs() {
            let opts = ReadOpts::parse(Some(&plain()), COMMA).unwrap();
            let mut scanned = CheckedInput::new(input.as_slice(), &opts);
            let mut bytes = Vec::new();
            if scanned.read_to_end(&mut bytes).is_err() {
                continue;
            }
            let mut parser = csv::ReaderBuilder::new()
                .has_headers(false)
                .flexible(true)
                .comment(Some(b'#'))
                .from_reader(bytes.as_slice());
            let mut records = parser.byte_records().count();
            // `csv` adds an empty record after a last comment with no newline.
            if matches!(scanned.state, ScanState::Comment) {
                records -= 1;
            }
            assert_eq!(
                records,
                scanned.record_lines.len(),
                "input {:?}",
                String::from_utf8_lossy(&input)
            );
        }
    }

    #[test]
    fn well_formed_text_parses_as_written() {
        let text = "a,b\r\n\"x,\"\"y\"\"\",\"1\n2\"\r\n";
        assert_eq!(
            read(text.as_bytes(), Val::map_empty()).unwrap(),
            vec![crate::val!({"a": "x,\"y\"", "b": "1\n2"})]
        );
    }

    #[test]
    fn a_row_past_the_limit_is_refused_without_reading_the_rest() {
        let mut input = CheckedInput::new(
            io::empty(),
            &ReadOpts::parse(None, COMMA).expect("default options"),
        );
        input.state = ScanState::FieldStart;
        input.row_bytes = limits::max_parse_input_bytes();
        let error = input.scan(b"more").expect_err("over the limit");
        assert!(error.to_string().contains("refusing to read a row"));
    }

    #[test]
    fn errors_hold_no_cell_values() {
        let opts = crate::val!({"types": {"a": "Int"}});
        let error = read(b"a\nprivate-value\n", opts).unwrap_err();
        assert!(error.contains("line 2, row 1"), "{error}");
        assert!(!error.contains("private-value"), "{error}");
        let error = read(b"a,b\nprivate-value\n", Val::map_empty()).unwrap_err();
        assert!(!error.contains("private-value"), "{error}");
        let error = read(b"a\nprivate-\"value\n", Val::map_empty()).unwrap_err();
        assert!(!error.contains("private"), "{error}");
    }

    #[test]
    fn a_failed_iterator_stays_failed() {
        let HotResult::Ok(Val::Box(boxed)) = iter_csv(&[Val::from("a,b\n1\n2,3\n")]) else {
            panic!("expected an iterator");
        };
        let iterator = boxed.as_any().downcast_ref::<IteratorBox>().unwrap();
        let mut rows = iterator.inner.lock().unwrap();
        let first = rows.next().unwrap_err();
        assert!(first.contains("line 2"), "{first}");
        assert_eq!(first, rows.next().unwrap_err());
    }

    #[test]
    fn a_failing_source_is_an_error_not_the_end_of_the_file() {
        #[derive(Debug)]
        struct Broken;
        impl HotIterator for Broken {
            fn next(&mut self) -> Result<(Val, bool), String> {
                Err("connection lost".to_string())
            }
        }
        let source: Box<dyn HotIterator> = Box::new(Broken);
        let opts = ReadOpts::parse(None, COMMA).unwrap();
        let mut reader =
            RowReader::new(ChunkInput::new(Arc::new(Mutex::new(source))), opts).unwrap();
        assert!(reader.next_row().unwrap_err().contains("connection lost"));
    }

    #[test]
    fn large_inputs_read_the_same_either_way() {
        let mut text = String::from("id,zip,note\n");
        for i in 0..50_000 {
            text.push_str(&format!("{i},{:05},\"a, \"\"b\"\" {i}\"\n", i % 1000));
        }
        let whole = read(text.as_bytes(), Val::map_empty()).unwrap();
        assert_eq!(whole.len(), 50_000);
        assert_eq!(
            whole,
            read_in_chunks(text.as_bytes(), Val::map_empty(), 4096).unwrap()
        );
        let HotResult::Ok(Val::Str(written)) = to_csv(&[Val::vec(whole.clone())]) else {
            panic!("to-csv failed");
        };
        assert_eq!(&*written, text);
    }
}
