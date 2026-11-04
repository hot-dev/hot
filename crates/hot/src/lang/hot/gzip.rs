// Gzip compression for Hot language, and the output limit ::hot::zip shares

use std::io::{BufReader, Read, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex};

use flate2::Compression;
use flate2::read::MultiGzDecoder;
use flate2::write::GzEncoder;

use crate::lang::hot::csv::{ChunkInput, option_entries};
use crate::lang::hot::iter::{HotIterator, IteratorBox, VecIterator};
use crate::lang::hot::r#type::HotResult;
use crate::lang::runtime::vm::VirtualMachine;
use crate::val::Val;
use crate::validate_args_range;

/// Largest output a call that returns `Bytes` may produce, unless
/// `compression.max-output-bytes` says otherwise.
pub(crate) const DEFAULT_MAX_OUTPUT_BYTES: usize = 64 * 1024 * 1024;
pub(crate) const MAX_OUTPUT_CONF: &str = "compression.max-output-bytes";

/// Bytes a decompressing stream yields at a time.
const STREAM_CHUNK_BYTES: usize = 64 * 1024;

pub(crate) fn configured_max_output(vm: &VirtualMachine) -> usize {
    vm.get_conf()
        .get_int_or_default(MAX_OUTPUT_CONF, DEFAULT_MAX_OUTPUT_BYTES as i64)
        .max(1) as usize
}

/// The `max-bytes` option, if given. Options come from the program, so they can
/// only lower the limit the host configures.
pub(crate) fn max_bytes_option(opts: Option<&Val>) -> Result<Option<usize>, String> {
    let mut max_bytes = None;
    for (name, value) in option_entries(opts, &["max-bytes"])? {
        match (name, value) {
            ("max-bytes", Val::Int(n)) if *n > 0 => max_bytes = Some(*n as usize),
            ("max-bytes", Val::Null) => {}
            _ => return Err("max-bytes must be a positive Int".to_string()),
        }
    }
    Ok(max_bytes)
}

/// Run third-party parsing of untrusted input, so a panic in it is an Err.
pub(crate) fn guarded<T>(work: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    catch_unwind(AssertUnwindSafe(work))
        .unwrap_or_else(|_| Err("input is corrupt: the decoder failed".to_string()))
}

/// Read to the end, refusing to hold more than `limit` bytes. The limit applies
/// to the bytes that come out, whatever the input claims about its size.
pub(crate) fn read_limited(mut reader: impl Read, limit: usize) -> Result<Vec<u8>, String> {
    let mut output = Vec::new();
    reader
        .by_ref()
        .take((limit as u64).saturating_add(1))
        .read_to_end(&mut output)
        .map_err(|error| error.to_string())?;
    if output.len() > limit {
        return Err(output_too_large(limit));
    }
    Ok(output)
}

pub(crate) fn output_too_large(limit: usize) -> String {
    format!(
        "output exceeds {limit} bytes, the limit from max-bytes or {MAX_OUTPUT_CONF}. A stream \
         can read more"
    )
}

/// Decompressed bytes, a chunk at a time, so memory stays flat however large
/// the output is.
pub(crate) struct DecodedChunks {
    op: &'static str,
    // A Mutex because a reader is Send but not Sync, as an iterator must be.
    reader: Mutex<Box<dyn Read + Send>>,
    limit: Option<usize>,
    produced: usize,
    done: bool,
    /// An error ends the stream for good. Later calls report it again, so
    /// nothing mistakes it for the end of the data.
    failed: Option<String>,
}

impl std::fmt::Debug for DecodedChunks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecodedChunks")
            .field("op", &self.op)
            .field("limit", &self.limit)
            .field("produced", &self.produced)
            .field("done", &self.done)
            .finish_non_exhaustive()
    }
}

impl DecodedChunks {
    pub(crate) fn new(
        op: &'static str,
        reader: Box<dyn Read + Send>,
        limit: Option<usize>,
    ) -> Self {
        DecodedChunks {
            op,
            reader: Mutex::new(reader),
            limit,
            produced: 0,
            done: false,
            failed: None,
        }
    }

    pub(crate) fn into_val(self) -> Val {
        Val::Box(Box::new(IteratorBox::new(Box::new(self))))
    }

    fn read_chunk(&mut self) -> Result<Option<Vec<u8>>, String> {
        let reader = self.reader.get_mut().map_err(|e| e.to_string())?;
        let mut chunk = vec![0u8; STREAM_CHUNK_BYTES];
        let read = guarded(|| reader.read(&mut chunk).map_err(|e| e.to_string()))?;
        if read == 0 {
            return Ok(None);
        }
        self.produced = self.produced.saturating_add(read);
        if let Some(limit) = self.limit
            && self.produced > limit
        {
            return Err(format!("output exceeds max-bytes ({limit})"));
        }
        chunk.truncate(read);
        Ok(Some(chunk))
    }
}

impl HotIterator for DecodedChunks {
    fn next(&mut self) -> Result<(Val, bool), String> {
        if let Some(error) = &self.failed {
            return Err(error.clone());
        }
        if self.done {
            return Ok((Val::Null, true));
        }
        match self.read_chunk() {
            Ok(Some(chunk)) => Ok((Val::bytes(chunk), false)),
            Ok(None) => {
                self.done = true;
                Ok((Val::Null, true))
            }
            Err(error) => {
                let error = format!("{}: {error}", self.op);
                self.failed = Some(error.clone());
                Err(error)
            }
        }
    }
}

fn parse_level(opts: Option<&Val>) -> Result<Compression, String> {
    let mut level = Compression::default();
    for (name, value) in option_entries(opts, &["level"])? {
        match (name, value) {
            ("level", Val::Int(n)) if (0..=9).contains(n) => level = Compression::new(*n as u32),
            ("level", Val::Null) => {}
            _ => return Err("level must be an Int from 0 to 9".to_string()),
        }
    }
    Ok(level)
}

/// Compress to gzip. The header holds no timestamp or file name, so the same
/// input always gives the same output.
pub fn encode(args: &[Val]) -> HotResult<Val> {
    validate_args_range!("::hot::gzip/encode", args, 1, 2);
    let fail =
        |message: String| HotResult::Err(Val::from(format!("::hot::gzip/encode: {message}")));
    let data: &[u8] = match &args[0] {
        Val::Bytes(bytes) => bytes,
        Val::Str(text) => text.as_bytes(),
        _ => return fail("expects Bytes or Str".to_string()),
    };
    let level = match parse_level(args.get(1)) {
        Ok(level) => level,
        Err(error) => return fail(error),
    };
    let mut encoder = GzEncoder::new(Vec::new(), level);
    match encoder.write_all(data).and_then(|_| encoder.finish()) {
        Ok(compressed) => HotResult::Ok(Val::bytes(compressed)),
        Err(error) => fail(error.to_string()),
    }
}

fn decode_limited(args: &[Val], ceiling: usize) -> HotResult<Val> {
    let fail =
        |message: String| HotResult::Err(Val::from(format!("::hot::gzip/decode: {message}")));
    let Val::Bytes(data) = &args[0] else {
        return fail("expects Bytes".to_string());
    };
    let limit = match max_bytes_option(args.get(1)) {
        Ok(max_bytes) => max_bytes.map_or(ceiling, |n| n.min(ceiling)),
        Err(error) => return fail(error),
    };
    if data.is_empty() {
        return fail("input is empty".to_string());
    }
    // MultiGzDecoder because a gzip file can hold several members, and gzip
    // tools read them all.
    match guarded(|| read_limited(MultiGzDecoder::new(data.as_slice()), limit)) {
        Ok(output) => HotResult::Ok(Val::bytes(output)),
        Err(error) => fail(error),
    }
}

pub fn decode(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_args_range!("::hot::gzip/decode", args, 1, 2);
    decode_limited(args, configured_max_output(vm))
}

pub fn decode_stream(args: &[Val]) -> HotResult<Val> {
    validate_args_range!("::hot::gzip/decode-stream", args, 1, 2);
    let fail = |message: String| {
        HotResult::Err(Val::from(format!("::hot::gzip/decode-stream: {message}")))
    };
    let source = match &args[0] {
        Val::Box(boxed) => match boxed.as_any().downcast_ref::<IteratorBox>() {
            Some(iterator) => Arc::clone(&iterator.inner),
            None => return fail("expects an Iter or Bytes".to_string()),
        },
        chunk @ Val::Bytes(_) => {
            let only: Box<dyn HotIterator> = Box::new(VecIterator::new(vec![chunk.clone()]));
            Arc::new(Mutex::new(only))
        }
        _ => return fail("expects an Iter or Bytes".to_string()),
    };
    let limit = match max_bytes_option(args.get(1)) {
        Ok(limit) => limit,
        Err(error) => return fail(error),
    };
    let decoder = MultiGzDecoder::new(BufReader::new(ChunkInput::new(source)));
    HotResult::Ok(
        DecodedChunks::new("::hot::gzip/decode-stream", Box::new(decoder), limit).into_val(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::hot::iter::IteratorBox;

    fn gz(data: &[u8]) -> Vec<u8> {
        match encode(&[Val::bytes(data.to_vec())]) {
            HotResult::Ok(Val::Bytes(bytes)) => bytes.to_vec(),
            other => panic!("encode failed: {other:?}"),
        }
    }

    fn decoded(data: Vec<u8>, ceiling: usize) -> Result<Vec<u8>, String> {
        match decode_limited(&[Val::bytes(data)], ceiling) {
            HotResult::Ok(Val::Bytes(bytes)) => Ok(bytes.to_vec()),
            HotResult::Err(error) => Err(error.to_string()),
            other => panic!("unexpected result: {other:?}"),
        }
    }

    fn drain(stream: &Val) -> Result<Vec<u8>, String> {
        let Val::Box(boxed) = stream else {
            panic!("expected an iterator, got {stream:?}");
        };
        let iterator = boxed.as_any().downcast_ref::<IteratorBox>().unwrap();
        let mut iterator = iterator.inner.lock().unwrap();
        let mut output = Vec::new();
        loop {
            let (chunk, done) = iterator.next()?;
            if done {
                return Ok(output);
            }
            match chunk {
                Val::Bytes(bytes) => output.extend_from_slice(&bytes),
                other => panic!("expected Bytes, got {other:?}"),
            }
        }
    }

    fn stream_of(data: Vec<u8>, opts: Option<Val>) -> Val {
        let mut args = vec![Val::bytes(data)];
        args.extend(opts);
        match decode_stream(&args) {
            HotResult::Ok(stream) => stream,
            other => panic!("decode-stream failed: {other:?}"),
        }
    }

    #[test]
    fn round_trips() {
        for data in [&b""[..], b"x", b"hello hello hello", &[0u8; 100_000]] {
            assert_eq!(decoded(gz(data), 1 << 20).unwrap(), data);
            assert_eq!(drain(&stream_of(gz(data), None)).unwrap(), data);
        }
    }

    #[test]
    fn output_is_deterministic() {
        assert_eq!(gz(b"same"), gz(b"same"));
    }

    #[test]
    fn reads_every_member() {
        let mut data = gz(b"one ");
        data.extend(gz(b"two"));
        assert_eq!(decoded(data.clone(), 100).unwrap(), b"one two");
        assert_eq!(drain(&stream_of(data, None)).unwrap(), b"one two");
    }

    #[test]
    fn a_bomb_stops_at_the_limit() {
        let bomb = gz(&vec![0u8; 8 * 1024 * 1024]);
        assert!(bomb.len() < 64 * 1024);
        let error = decoded(bomb.clone(), 1024).unwrap_err();
        assert!(error.contains("output exceeds 1024 bytes"), "{error}");
        assert_eq!(decoded(gz(&[0u8; 1024]), 1024).unwrap().len(), 1024);
        assert!(decoded(gz(&[0u8; 1025]), 1024).is_err());

        let opts = Val::map_from_iter([(Val::from("max-bytes"), Val::Int(1024))]);
        let error = drain(&stream_of(bomb, Some(opts))).unwrap_err();
        assert!(error.contains("max-bytes"), "{error}");
    }

    #[test]
    fn the_option_only_lowers_the_ceiling() {
        let opts = Val::map_from_iter([(Val::from("max-bytes"), Val::Int(1 << 30))]);
        let result = decode_limited(&[Val::bytes(gz(&[0u8; 2048])), opts], 1024);
        assert!(matches!(result, HotResult::Err(_)), "{result:?}");
    }

    #[test]
    fn corrupt_input_is_an_err() {
        let good = gz(b"the quick brown fox jumps over the lazy dog");
        assert!(decoded(Vec::new(), 1024).is_err());
        assert!(decoded(b"not gzip at all".to_vec(), 1024).is_err());
        assert!(drain(&stream_of(Vec::new(), None)).is_err());
        assert!(drain(&stream_of(b"not gzip".to_vec(), None)).is_err());

        for end in 0..good.len() {
            assert!(decoded(good[..end].to_vec(), 1024).is_err(), "cut at {end}");
            assert!(drain(&stream_of(good[..end].to_vec(), None)).is_err());
        }

        // The last eight bytes are the checksum and the length.
        let mut bad_checksum = good.clone();
        let last = bad_checksum.len() - 8;
        bad_checksum[last] ^= 0xff;
        assert!(decoded(bad_checksum.clone(), 1024).is_err());
        assert!(drain(&stream_of(bad_checksum, None)).is_err());

        let mut trailing = good.clone();
        trailing.extend_from_slice(b"junk");
        assert!(decoded(trailing, 1024).is_err());

        // Any flipped byte may still decode, but must not panic.
        for index in 0..good.len() {
            let mut flipped = good.clone();
            flipped[index] ^= 0x55;
            let _ = decoded(flipped, 1024);
        }
    }

    #[test]
    fn a_failed_stream_stays_failed() {
        let stream = stream_of(b"not gzip".to_vec(), None);
        let Val::Box(boxed) = &stream else {
            panic!("expected an iterator");
        };
        let iterator = boxed.as_any().downcast_ref::<IteratorBox>().unwrap();
        let mut iterator = iterator.inner.lock().unwrap();
        let first = iterator.next().unwrap_err();
        assert_eq!(iterator.next().unwrap_err(), first);
    }

    #[test]
    fn rejects_bad_options() {
        let bad = Val::map_from_iter([(Val::from("level"), Val::Int(10))]);
        assert!(matches!(encode(&[Val::from("x"), bad]), HotResult::Err(_)));
        let unknown = Val::map_from_iter([(Val::from("max-byte"), Val::Int(1))]);
        assert!(matches!(
            decode_limited(&[Val::bytes(gz(b"x")), unknown], 1024),
            HotResult::Err(_)
        ));
        let zero = Val::map_from_iter([(Val::from("max-bytes"), Val::Int(0))]);
        assert!(matches!(
            decode_limited(&[Val::bytes(gz(b"x")), zero], 1024),
            HotResult::Err(_)
        ));
    }
}
