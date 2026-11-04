// Zip archives for Hot language

use std::io::{self, Cursor, Read, Seek, SeekFrom, Write};
use std::sync::Arc;

use ::zip::read::ZipReadOptions;
use ::zip::write::SimpleFileOptions;
use ::zip::{CompressionMethod, DateTime, ZipArchive, ZipWriter};
use flate2::read::DeflateDecoder;

use crate::lang::hot::file::{self, SeekableFile};
use crate::lang::hot::gzip::{
    DecodedChunks, MAX_OUTPUT_CONF, configured_max_output, guarded, max_bytes_option,
    output_too_large, read_limited,
};
use crate::lang::hot::r#type::HotResult;
use crate::lang::runtime::limits;
use crate::lang::runtime::vm::VirtualMachine;
use crate::val::Val;
use crate::{validate_args, validate_args_range};

/// Bytes a [`Val`] already holds, shared instead of copied.
struct SharedBytes(Arc<Vec<u8>>);

impl AsRef<[u8]> for SharedBytes {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

type Archive = ZipArchive<Box<dyn SeekableFile>>;

fn open_bytes(bytes: &Arc<Vec<u8>>) -> Box<dyn SeekableFile> {
    Box::new(Cursor::new(SharedBytes(Arc::clone(bytes))))
}

/// An archive is its Bytes, or a Str naming a file. A file is read in place, so
/// an archive too big to hold in memory can still be read.
fn open_source(vm: &mut VirtualMachine, source: &Val) -> Result<Box<dyn SeekableFile>, String> {
    match source {
        Val::Bytes(bytes) => Ok(open_bytes(bytes)),
        Val::Str(path) => file::open_seekable(vm, path),
        _ => Err("expects Bytes or a file path".to_string()),
    }
}

fn open_archive(source: Box<dyn SeekableFile>) -> Result<Archive, String> {
    guarded(|| ZipArchive::new(source).map_err(|e| format!("not a valid zip archive: {e}")))
}

/// The relative path `name` stands for, or null when writing it out could land
/// outside the target directory. This is stricter than the platform it runs
/// on needs, so a path that's safe here is safe everywhere: no `..`, no root,
/// no drive or stream (`:`), no backslash.
fn safe_path(name: &str) -> Option<String> {
    if name.contains(['\0', '\\', ':']) || name.starts_with('/') {
        return None;
    }
    let mut parts = Vec::new();
    for part in name.split('/') {
        match part {
            "" | "." => {}
            ".." => return None,
            part => parts.push(part),
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

fn method_name(method: CompressionMethod) -> String {
    match method {
        CompressionMethod::Stored => "store".to_string(),
        CompressionMethod::Deflated => "deflate".to_string(),
        CompressionMethod::Bzip2 => "bzip2".to_string(),
        CompressionMethod::Zstd => "zstd".to_string(),
        other => format!("{other:?}").to_lowercase(),
    }
}

fn entry_size(size: u64) -> Result<Val, String> {
    i64::try_from(size)
        .map(Val::Int)
        .map_err(|_| format!("not a valid zip archive: an entry claims {size} bytes"))
}

fn list_entries(archive: &Archive) -> Result<Val, String> {
    limits::check_collection_size("::hot::zip/entries", archive.len())
        .map_err(|error| error.to_string())?;
    let mut entries = Vec::with_capacity(archive.len());
    for index in 0..archive.len() {
        let entry = archive
            .by_index_data(index)
            .map_err(|e| format!("not a valid zip archive: {e}"))?;
        let name = entry
            .name()
            .map_err(|e| format!("not a valid zip archive: {e}"))?
            .to_string();
        let path = safe_path(&name).map_or(Val::Null, Val::from);
        entries.push(Val::map_from_iter([
            (Val::from("name"), Val::from(name)),
            (Val::from("safe-path"), path),
            (Val::from("size"), entry_size(entry.size())?),
            (
                Val::from("compressed-size"),
                entry_size(entry.compressed_size())?,
            ),
            (
                Val::from("method"),
                Val::from(method_name(entry.compression())),
            ),
            (Val::from("is-dir"), Val::Bool(entry.is_dir())),
            (Val::from("is-encrypted"), Val::Bool(entry.encrypted())),
        ]));
    }
    Ok(Val::vec(entries))
}

/// Refuses an archive whose central directory and local headers disagree, or
/// that has two entries of one name: tools would read different files from it.
fn read_options<'a>() -> ZipReadOptions<'a> {
    ZipReadOptions::new().enforce_header_consistency(true)
}

fn entry_index(archive: &Archive, name: &str) -> Result<usize, String> {
    archive
        .index_for_name(name)
        .ok_or_else(|| format!("no entry named {name:?}"))
}

fn read_entry_limited(mut archive: Archive, name: &str, limit: usize) -> Result<Vec<u8>, String> {
    let index = entry_index(&archive, name)?;
    let mut entry = archive
        .by_index_with_options(index, read_options())
        .map_err(|e| format!("can't read {name:?}: {e}"))?;
    // Say no before decompressing anything when the archive is honest. A
    // dishonest size is caught below, as the bytes come out.
    let declared = entry.size();
    if declared > limit as u64 {
        return Err(output_too_large(limit));
    }
    let output = read_limited(&mut entry, limit).map_err(|e| {
        if e.contains(MAX_OUTPUT_CONF) {
            e
        } else {
            format!("can't read {name:?}: {e}")
        }
    })?;
    if output.len() as u64 != declared {
        return Err(format!(
            "can't read {name:?}: it holds {} bytes, not the {declared} its header claims",
            output.len()
        ));
    }
    Ok(output)
}

/// An entry's bytes as they decompress, checked against what the archive
/// declares for it. The check ends the stream with an error, so a consumer
/// can't take a truncated or altered entry for a whole one.
struct CheckedEntry<R> {
    inner: R,
    expected_size: u64,
    expected_crc: u32,
    read: u64,
    hasher: crc32fast::Hasher,
}

impl<R: Read> Read for CheckedEntry<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let invalid = |message: String| io::Error::new(io::ErrorKind::InvalidData, message);
        let read = self.inner.read(buf)?;
        self.read = self.read.saturating_add(read as u64);
        if self.read > self.expected_size {
            return Err(invalid(format!(
                "entry holds more than the {} bytes its header claims",
                self.expected_size
            )));
        }
        self.hasher.update(&buf[..read]);
        if read == 0 && !buf.is_empty() {
            if self.read != self.expected_size {
                return Err(invalid(format!(
                    "entry holds {} bytes, not the {} its header claims",
                    self.read, self.expected_size
                )));
            }
            if self.hasher.clone().finalize() != self.expected_crc {
                return Err(invalid("entry fails its checksum".to_string()));
            }
        }
        Ok(read)
    }
}

/// Opens an entry for streaming. The archive's own reader borrows the archive,
/// and an iterator has to own what it reads, so this reads the entry's
/// compressed bytes directly. Deflate and store are what zip files use in
/// practice; anything else can still go through `read-entry`.
fn open_entry_stream(
    mut archive: Archive,
    name: &str,
    limit: Option<usize>,
) -> Result<Box<dyn Read + Send>, String> {
    let index = entry_index(&archive, name)?;
    let entry = archive
        .by_index_with_options(index, read_options())
        .map_err(|e| format!("can't read {name:?}: {e}"))?;
    let method = entry.compression();
    let (size, compressed_size, crc) = (entry.size(), entry.compressed_size(), entry.crc32());
    let start = entry
        .data_start()
        .ok_or_else(|| format!("can't read {name:?}: its data can't be located"))?;
    drop(entry);
    if !matches!(
        method,
        CompressionMethod::Stored | CompressionMethod::Deflated
    ) {
        return Err(format!(
            "can't stream {name:?}: it uses {}. read-entry can read it",
            method_name(method)
        ));
    }
    if let Some(limit) = limit
        && size > limit as u64
    {
        return Err(format!("output exceeds max-bytes ({limit})"));
    }
    let mut source = archive.into_inner();
    source
        .seek(SeekFrom::Start(start))
        .map_err(|e| format!("can't read {name:?}: {e}"))?;
    let compressed = source.take(compressed_size);
    let inner: Box<dyn Read + Send> = match method {
        CompressionMethod::Stored => Box::new(compressed),
        _ => Box::new(DeflateDecoder::new(compressed)),
    };
    Ok(Box::new(CheckedEntry {
        inner,
        expected_size: size,
        expected_crc: crc,
        read: 0,
        hasher: crc32fast::Hasher::new(),
    }))
}

fn fail(op: &str, message: String) -> HotResult<Val> {
    HotResult::Err(Val::from(format!("{op}: {message}")))
}

pub fn entries(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    const OP: &str = "::hot::zip/entries";
    validate_args!(OP, args, 1);
    let listed = open_source(vm, &args[0])
        .and_then(open_archive)
        .and_then(|archive| guarded(|| list_entries(&archive)));
    match listed {
        Ok(list) => HotResult::Ok(list),
        Err(error) => fail(OP, error),
    }
}

fn entry_name(args: &[Val]) -> Result<&str, String> {
    match &args[1] {
        Val::Str(name) => Ok(name),
        _ => Err("entry name must be a Str".to_string()),
    }
}

pub fn read_entry(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    const OP: &str = "::hot::zip/read-entry";
    validate_args_range!(OP, args, 2, 3);
    let ceiling = configured_max_output(vm);
    let read = (|| {
        let name = entry_name(args)?;
        let limit = max_bytes_option(args.get(2))?.map_or(ceiling, |n| n.min(ceiling));
        let archive = open_archive(open_source(vm, &args[0])?)?;
        guarded(|| read_entry_limited(archive, name, limit))
    })();
    match read {
        Ok(bytes) => HotResult::Ok(Val::bytes(bytes)),
        Err(error) => fail(OP, error),
    }
}

pub fn read_entry_stream(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    const OP: &str = "::hot::zip/read-entry-stream";
    validate_args_range!(OP, args, 2, 3);
    let opened = (|| {
        let name = entry_name(args)?;
        let limit = max_bytes_option(args.get(2))?;
        let archive = open_archive(open_source(vm, &args[0])?)?;
        let reader = guarded(|| open_entry_stream(archive, name, limit))?;
        Ok::<_, String>(DecodedChunks::new(OP, reader, limit))
    })();
    match opened {
        Ok(stream) => HotResult::Ok(stream.into_val()),
        Err(error) => fail(OP, error),
    }
}

fn add_entry(writer: &mut ZipWriter<Cursor<Vec<u8>>>, entry: &Val) -> Result<(), String> {
    let Val::Map(entry) = entry else {
        return Err("each entry must be a Map with name and data".to_string());
    };
    let Some(Val::Str(name)) = entry.get(&Val::from("name")) else {
        return Err("each entry needs a Str name".to_string());
    };
    if safe_path(name).is_none() || name.ends_with('/') {
        return Err(format!("unsafe entry name {name:?}"));
    }
    let data: &[u8] = match entry.get(&Val::from("data")) {
        Some(Val::Bytes(bytes)) => bytes,
        Some(Val::Str(text)) => text.as_bytes(),
        _ => return Err(format!("entry {name:?} needs data as Bytes or Str")),
    };
    // A fixed timestamp, so the same entries always give the same archive.
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .last_modified_time(DateTime::default());
    writer
        .start_file(name.to_string(), options)
        .map_err(|e| format!("can't add {name:?}: {e}"))?;
    writer
        .write_all(data)
        .map_err(|e| format!("can't add {name:?}: {e}"))
}

pub fn create(args: &[Val]) -> HotResult<Val> {
    const OP: &str = "::hot::zip/create";
    validate_args!(OP, args, 1);
    let Val::Vec(entries) = &args[0] else {
        return fail(OP, "expects a Vec of entries".to_string());
    };
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    for entry in entries.iter() {
        if let Err(error) = add_entry(&mut writer, entry) {
            return fail(OP, error);
        }
    }
    match writer.finish() {
        Ok(cursor) => HotResult::Ok(Val::bytes(cursor.into_inner())),
        Err(error) => fail(OP, error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::hot::iter::IteratorBox;
    use crate::lang::runtime::vm::VirtualMachine;

    fn sample(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let entries = entries
            .iter()
            .map(|(name, data)| {
                Val::map_from_iter([
                    (Val::from("name"), Val::from(*name)),
                    (Val::from("data"), Val::bytes(data.to_vec())),
                ])
            })
            .collect();
        match create(&[Val::vec(entries)]) {
            HotResult::Ok(Val::Bytes(bytes)) => bytes.to_vec(),
            other => panic!("create failed: {other:?}"),
        }
    }

    fn archive(bytes: Vec<u8>) -> Result<Archive, String> {
        open_archive(open_bytes(&Arc::new(bytes)))
    }

    fn read(bytes: Vec<u8>, name: &str, limit: usize) -> Result<Vec<u8>, String> {
        guarded(|| read_entry_limited(archive(bytes)?, name, limit))
    }

    fn stream(bytes: Vec<u8>, name: &str, limit: Option<usize>) -> Result<Vec<u8>, String> {
        let reader = guarded(|| open_entry_stream(archive(bytes)?, name, limit))?;
        let chunks = DecodedChunks::new("test", reader, limit).into_val();
        let Val::Box(boxed) = &chunks else {
            panic!("expected an iterator");
        };
        let iterator = boxed.as_any().downcast_ref::<IteratorBox>().unwrap();
        let mut iterator = iterator.inner.lock().unwrap();
        let mut output = Vec::new();
        loop {
            let (chunk, done) = iterator.next()?;
            if done {
                return Ok(output);
            }
            let Val::Bytes(bytes) = chunk else {
                panic!("expected Bytes");
            };
            output.extend_from_slice(&bytes);
        }
    }

    /// Finds the local and central headers of the only entry in `bytes`.
    fn headers(bytes: &[u8]) -> (usize, usize) {
        let find = |magic: &[u8]| bytes.windows(4).position(|w| w == magic).unwrap();
        (find(b"PK\x03\x04"), find(b"PK\x01\x02"))
    }

    fn put_u32(bytes: &mut [u8], at: usize, value: u32) {
        bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }

    /// Rewrites the uncompressed size an archive declares for its only entry,
    /// in both headers, so the headers still agree with each other.
    fn with_declared_size(mut bytes: Vec<u8>, size: u32) -> Vec<u8> {
        let (local, central) = headers(&bytes);
        put_u32(&mut bytes, local + 22, size);
        put_u32(&mut bytes, central + 24, size);
        bytes
    }

    #[test]
    fn round_trips() {
        let data = b"a,b\n1,2\n".repeat(1000);
        let empty: &[u8] = b"";
        let bytes = sample(&[("dir/data.csv", &data), ("empty.txt", empty)]);
        assert_eq!(read(bytes.clone(), "dir/data.csv", 1 << 20).unwrap(), data);
        assert_eq!(read(bytes.clone(), "empty.txt", 1).unwrap(), empty);
        assert_eq!(stream(bytes.clone(), "dir/data.csv", None).unwrap(), data);
        assert_eq!(stream(bytes.clone(), "empty.txt", None).unwrap(), empty);

        let listed = list_entries(&archive(bytes).unwrap()).unwrap();
        let Val::Vec(listed) = listed else {
            panic!("expected a Vec");
        };
        let Val::Map(first) = &listed[0] else {
            panic!("expected a Map");
        };
        assert_eq!(
            first.get(&Val::from("name")),
            Some(&Val::from("dir/data.csv"))
        );
        assert_eq!(
            first.get(&Val::from("safe-path")),
            Some(&Val::from("dir/data.csv"))
        );
        assert_eq!(
            first.get(&Val::from("size")),
            Some(&Val::Int(data.len() as i64))
        );
        assert_eq!(first.get(&Val::from("method")), Some(&Val::from("deflate")));
    }

    #[test]
    fn output_is_deterministic() {
        let entries: &[(&str, &[u8])] = &[("a.txt", b"a"), ("b.txt", b"b")];
        assert_eq!(sample(entries), sample(entries));
    }

    #[test]
    fn reads_a_stored_entry() {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .start_file(
                "plain.txt",
                SimpleFileOptions::default().compression_method(CompressionMethod::Stored),
            )
            .unwrap();
        writer.write_all(b"stored bytes").unwrap();
        let bytes = writer.finish().unwrap().into_inner();
        assert_eq!(
            read(bytes.clone(), "plain.txt", 100).unwrap(),
            b"stored bytes"
        );
        assert_eq!(stream(bytes, "plain.txt", None).unwrap(), b"stored bytes");
    }

    /// FFIEC's Call Report zips carry each entry's sizes in zip64 fields.
    #[test]
    fn reads_zip64_sizes() {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .start_file("a.txt", SimpleFileOptions::default().large_file(true))
            .unwrap();
        writer.write_all(b"zip64 sizes").unwrap();
        let bytes = writer.finish().unwrap().into_inner();
        assert_eq!(read(bytes.clone(), "a.txt", 100).unwrap(), b"zip64 sizes");
        assert_eq!(stream(bytes, "a.txt", None).unwrap(), b"zip64 sizes");
    }

    /// SEC's Form ADV zips were written as streams: each local header has no
    /// sizes, and a descriptor after the data holds them.
    #[test]
    fn reads_data_descriptors() {
        let mut writer = ZipWriter::new_stream(Vec::new());
        writer
            .start_file("a.txt", SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"streamed entry").unwrap();
        writer
            .start_file("b.txt", SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"another").unwrap();
        let bytes = writer.finish().unwrap().into_inner();
        assert_eq!(
            read(bytes.clone(), "a.txt", 100).unwrap(),
            b"streamed entry"
        );
        assert_eq!(
            stream(bytes.clone(), "a.txt", None).unwrap(),
            b"streamed entry"
        );
        assert_eq!(stream(bytes, "b.txt", None).unwrap(), b"another");
    }

    #[test]
    fn a_missing_entry_is_an_err() {
        let bytes = sample(&[("a.txt", b"a")]);
        assert!(
            read(bytes.clone(), "b.txt", 100)
                .unwrap_err()
                .contains("no entry")
        );
        assert!(stream(bytes, "b.txt", None).is_err());
    }

    #[test]
    fn an_honest_bomb_is_refused_before_decompressing() {
        let bytes = sample(&[("bomb", &vec![0u8; 4 * 1024 * 1024])]);
        assert!(bytes.len() < 64 * 1024);
        let error = read(bytes.clone(), "bomb", 1024).unwrap_err();
        assert!(error.contains("output exceeds 1024 bytes"), "{error}");
        assert!(stream(bytes.clone(), "bomb", Some(1024)).is_err());
        assert_eq!(stream(bytes, "bomb", None).unwrap().len(), 4 * 1024 * 1024);
    }

    #[test]
    fn a_bomb_that_lies_about_its_size_stops_at_the_limit() {
        let bytes = with_declared_size(sample(&[("bomb", &vec![0u8; 4 * 1024 * 1024])]), 10);
        let error = read(bytes.clone(), "bomb", 1024).unwrap_err();
        assert!(error.contains("larger than its declared"), "{error}");
        let error = stream(bytes, "bomb", None).unwrap_err();
        assert!(error.contains("more than the 10 bytes"), "{error}");
    }

    #[test]
    fn a_wrong_size_or_checksum_is_an_err() {
        let good = sample(&[("a.txt", b"hello hello hello")]);
        let too_big = with_declared_size(good.clone(), 99);
        assert!(read(too_big.clone(), "a.txt", 1024).is_err());
        assert!(stream(too_big, "a.txt", None).is_err());

        let mut bad_crc = good.clone();
        let (local, central) = headers(&bad_crc);
        put_u32(&mut bad_crc, local + 14, 0xdead_beef);
        put_u32(&mut bad_crc, central + 16, 0xdead_beef);
        assert!(read(bad_crc.clone(), "a.txt", 1024).is_err());
        assert!(stream(bad_crc, "a.txt", None).is_err());
    }

    #[test]
    fn headers_that_disagree_are_an_err() {
        let mut bytes = sample(&[("a.txt", b"hello")]);
        let (local, _) = headers(&bytes);
        bytes[local + 30] = b'b'; // The local header now says b.txt.
        assert!(read(bytes.clone(), "a.txt", 1024).is_err());
        assert!(stream(bytes, "a.txt", None).is_err());
    }

    #[test]
    fn corrupt_input_is_an_err() {
        assert!(archive(Vec::new()).is_err());
        assert!(archive(b"not a zip file".to_vec()).is_err());

        let good = sample(&[("a.txt", b"the quick brown fox"), ("b.txt", b"jumps over")]);
        for end in 0..good.len() {
            let cut = good[..end].to_vec();
            assert!(read(cut.clone(), "a.txt", 1024).is_err(), "cut at {end}");
            assert!(stream(cut, "a.txt", None).is_err(), "cut at {end}");
        }
        // Any flipped byte may still read, but must not panic.
        for index in 0..good.len() {
            for mask in [0x01, 0x55, 0xff] {
                let mut flipped = good.clone();
                flipped[index] ^= mask;
                let _ = read(flipped.clone(), "a.txt", 1024);
                let _ = stream(flipped.clone(), "b.txt", None);
                if let Ok(archive) = archive(flipped) {
                    let _ = list_entries(&archive);
                }
            }
        }
    }

    #[test]
    fn safe_paths() {
        for (name, expected) in [
            ("a.txt", Some("a.txt")),
            ("dir/a.txt", Some("dir/a.txt")),
            ("./dir//a.txt", Some("dir/a.txt")),
            ("dir/", Some("dir")),
            ("a..b/c..", Some("a..b/c..")),
            ("../a.txt", None),
            ("dir/../../a.txt", None),
            ("dir/../a.txt", None),
            ("/etc/passwd", None),
            ("..\\a.txt", None),
            ("C:\\a.txt", None),
            ("C:/a.txt", None),
            ("a.txt:stream", None),
            ("a\0.txt", None),
            ("", None),
            (".", None),
            ("/", None),
        ] {
            assert_eq!(safe_path(name).as_deref(), expected, "{name:?}");
        }
    }

    #[test]
    fn listing_shows_a_name_that_isnt_safe() {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .start_file("../evil.txt", SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"x").unwrap();
        let bytes = writer.finish().unwrap().into_inner();
        let Val::Vec(listed) = list_entries(&archive(bytes).unwrap()).unwrap() else {
            panic!("expected a Vec");
        };
        let Val::Map(first) = &listed[0] else {
            panic!("expected a Map");
        };
        assert_eq!(
            first.get(&Val::from("name")),
            Some(&Val::from("../evil.txt"))
        );
        assert_eq!(first.get(&Val::from("safe-path")), Some(&Val::Null));
    }

    #[test]
    fn create_refuses_what_it_should() {
        let entry = |name: &str| {
            Val::map_from_iter([
                (Val::from("name"), Val::from(name)),
                (Val::from("data"), Val::from("x")),
            ])
        };
        for name in ["../a.txt", "/a.txt", "a\\b.txt", "dir/", ""] {
            let result = create(&[Val::vec(vec![entry(name)])]);
            assert!(matches!(result, HotResult::Err(_)), "{name:?}");
        }
        let twice = create(&[Val::vec(vec![entry("a.txt"), entry("a.txt")])]);
        assert!(matches!(twice, HotResult::Err(_)));
        assert!(matches!(create(&[Val::from("x")]), HotResult::Err(_)));
    }

    fn vm_with(conf: Option<Val>) -> VirtualMachine {
        let compiler = crate::lang::compiler::Compiler::new();
        VirtualMachine::new(
            compiler.get_program_arc(),
            None,
            compiler.get_function_mapping_arc(),
            compiler.get_core_functions_arc(),
            compiler.get_type_implementations_arc(),
            compiler.get_core_variables_arc(),
            conf,
        )
    }

    #[test]
    fn the_configured_limit_applies() {
        let conf = Val::map_from_iter([(
            Val::from("compression"),
            Val::map_from_iter([(Val::from("max-output-bytes"), Val::Int(1024))]),
        )]);
        let mut limited = vm_with(Some(conf));
        let mut default = vm_with(None);
        let args = [
            Val::bytes(sample(&[("a.txt", &[0u8; 2048])])),
            Val::from("a.txt"),
        ];
        assert!(matches!(read_entry(&mut default, &args), HotResult::Ok(_)));
        let HotResult::Err(error) = read_entry(&mut limited, &args) else {
            panic!("expected an Err");
        };
        assert!(error.to_string().contains("output exceeds 1024"), "{error}");

        // The option can't raise it.
        let opts = Val::map_from_iter([(Val::from("max-bytes"), Val::Int(1 << 20))]);
        let raised = [args[0].clone(), args[1].clone(), opts];
        assert!(matches!(
            read_entry(&mut limited, &raised),
            HotResult::Err(_)
        ));
    }

    #[test]
    fn reads_an_archive_from_a_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("a.zip");
        std::fs::write(&path, sample(&[("a.txt", b"from a file")])).unwrap();
        let mut vm = vm_with(None);
        let source = Val::from(path.to_str().unwrap());

        let listed = entries(&mut vm, std::slice::from_ref(&source));
        assert!(matches!(listed, HotResult::Ok(Val::Vec(_))), "{listed:?}");
        let args = [source, Val::from("a.txt")];
        let HotResult::Ok(Val::Bytes(bytes)) = read_entry(&mut vm, &args) else {
            panic!("read-entry failed");
        };
        assert_eq!(bytes.as_slice(), b"from a file");
        assert!(matches!(
            read_entry_stream(&mut vm, &args),
            HotResult::Ok(_)
        ));

        let missing = Val::from(dir.path().join("none.zip").to_str().unwrap());
        assert!(matches!(entries(&mut vm, &[missing]), HotResult::Err(_)));
    }
}
