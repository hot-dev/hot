use crate::data::serialization::Serialization;
use crate::val::Val;
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};
use uuid::Uuid;
use zstd::{Decoder, Encoder};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Message {
    pub id: Uuid,
    pub head: Val,
    pub body: Val,
}

impl Message {
    /// Serialize the message using the specified format
    pub fn to_compressed_bytes(&self, format: Serialization) -> io::Result<Vec<u8>> {
        match format {
            Serialization::Json => {
                // Raw JSON serialization without compression
                serde_json::to_vec(self).map_err(|e| io::Error::other(e.to_string()))
            }
            Serialization::ZstdJson => {
                // Serialize to JSON first
                let serialized =
                    serde_json::to_vec(self).map_err(|e| io::Error::other(e.to_string()))?;

                // Then compress with zstd level 6
                let mut compressed = Vec::new();
                {
                    let mut encoder = Encoder::new(&mut compressed, 6)?; // Compression level 6
                    encoder.write_all(&serialized)?;
                    encoder.finish()?;
                }

                Ok(compressed)
            }
        }
    }

    /// Serialize using the default format (JSON) and compress
    pub fn to_compressed_bytes_default(&self) -> io::Result<Vec<u8>> {
        self.to_compressed_bytes(Serialization::default())
    }

    /// Deserialize from data with the specified format
    pub fn from_compressed_bytes(bytes: &[u8], format: Serialization) -> io::Result<Self> {
        match format {
            Serialization::Json => {
                // Raw JSON deserialization without decompression
                let message =
                    serde_json::from_slice(bytes).map_err(|e| io::Error::other(e.to_string()))?;
                Ok(message)
            }
            Serialization::ZstdJson => {
                // First decompress with zstd
                let mut decompressed = Vec::new();
                {
                    let mut decoder = Decoder::new(bytes)?;
                    decoder.read_to_end(&mut decompressed)?;
                }

                // Deserialize from JSON
                let message = serde_json::from_slice(&decompressed)
                    .map_err(|e| io::Error::other(e.to_string()))?;

                Ok(message)
            }
        }
    }

    /// Deserialize using the default format (JSON)
    pub fn from_compressed_bytes_default(bytes: &[u8]) -> io::Result<Self> {
        Self::from_compressed_bytes(bytes, Serialization::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compressed_serialization() {
        let message = Message {
            id: Uuid::now_v7(),
            head: crate::val!({
                "content-type": "application/json",
                "priority": 1
            }),
            body: crate::val!({
                "name": "test message",
                "data": [1, 2, 3, 4, 5]
            }),
        };

        // Test with JSON format (uncompressed)
        let json_bytes = message.to_compressed_bytes(Serialization::Json).unwrap();
        let json_decoded =
            Message::from_compressed_bytes(&json_bytes, Serialization::Json).unwrap();

        // Test with ZstdJson format
        let zstd_json_compressed = message
            .to_compressed_bytes(Serialization::ZstdJson)
            .unwrap();
        let zstd_json_decoded =
            Message::from_compressed_bytes(&zstd_json_compressed, Serialization::ZstdJson).unwrap();

        // Test default methods
        let default_compressed = message.to_compressed_bytes_default().unwrap();
        let default_decoded = Message::from_compressed_bytes_default(&default_compressed).unwrap();

        // Check that fields match for JSON format
        assert_eq!(message.id, json_decoded.id);
        assert_eq!(message.head, json_decoded.head);
        assert_eq!(message.body, json_decoded.body);

        // Check that fields match for ZstdJson format
        assert_eq!(message.id, zstd_json_decoded.id);
        assert_eq!(message.head, zstd_json_decoded.head);
        assert_eq!(message.body, zstd_json_decoded.body);

        // Check that fields match for default format
        assert_eq!(message.id, default_decoded.id);
        assert_eq!(message.head, default_decoded.head);
        assert_eq!(message.body, default_decoded.body);

        // Print compression comparison
        println!("Compression comparison:");
        println!("  JSON (raw):        {} bytes", json_bytes.len());
        println!("  ZstdJson:          {} bytes", zstd_json_compressed.len());
    }

    #[test]
    fn benchmark_serialization_sizes() {
        // This benchmark compares different serialization methods:
        // 1. Json: Raw JSON without compression
        // 2. ZstdJson: JSON with Zstd compression level 6

        println!("\n=== Message Serialization Size Benchmark ===");
        println!(
            "+-----------+---------------+-------------------+-------------------+-----------+"
        );
        println!(
            "| Data Type | JSON Size (B) | ZstdJson Size(B)  | ZstdJson Ratio     | Best Comp |"
        );
        println!(
            "+-----------+---------------+-------------------+-------------------+-----------+"
        );

        // Test with different data types to show compression characteristics:

        // 1. String data - typically highly compressible
        let string_data = create_body_with_strings(2000);
        let string_message = create_test_message("Strings", 2000, &string_data);
        print_compression_stats("Strings", &string_message);

        // 2. Numeric data - less compressible than strings
        let numeric_data = create_body_with_numbers(2000);
        let numeric_message = create_test_message("Numbers", 2000, &numeric_data);
        print_compression_stats("Numbers", &numeric_message);

        // 3. Mixed data - realistic mix of different types
        let mixed_data = create_body_with_mixed_data(2000);
        let mixed_message = create_test_message("Mixed", 2000, &mixed_data);
        print_compression_stats("Mixed", &mixed_message);

        // 4. Repeating data - shows maximum possible compression
        let repeating_data = create_body_with_repeating_data(2000);
        let repeating_message = create_test_message("Repeating", 2000, &repeating_data);
        print_compression_stats("Repeating", &repeating_message);

        println!(
            "+-----------+---------------+-------------------+-------------------+-----------+"
        );
        println!("=== End of Benchmark ===\n");
        println!("Notes:");
        println!("- Compression ratios show improvement vs raw JSON (higher is better)");
        println!("- 'Best Comp' shows which format achieved smallest size");
        println!("- ZstdJson provides best compression");
    }

    #[test]
    fn benchmark_compression_timing() {
        use std::time::Instant;

        // Create different sized test datasets for more comprehensive timing
        let test_cases = [
            ("Small (1K items)", 1000),
            ("Medium (5K items)", 5000),
            ("Large (10K items)", 10000),
        ];

        // Number of iterations for timing (fewer for larger datasets)
        const SMALL_ITERATIONS: usize = 50;
        const MEDIUM_ITERATIONS: usize = 20;
        const LARGE_ITERATIONS: usize = 10;

        println!("\n=== Message Compression Timing Benchmark ===");

        for (case_name, item_count) in test_cases.iter() {
            let iterations = match *item_count {
                1000 => SMALL_ITERATIONS,
                5000 => MEDIUM_ITERATIONS,
                _ => LARGE_ITERATIONS,
            };

            // Create test message with mixed data (most realistic)
            let test_data = create_body_with_mixed_data(*item_count as i64);
            let message = create_test_message("Timing", *item_count as i64, &test_data);

            // Get raw JSON size for reference
            let raw_json = serde_json::to_vec(&message).unwrap();
            let raw_json_size = raw_json.len();

            println!("\n--- {} Dataset ---", case_name);
            println!(
                "Raw JSON size: {:.2} KB, {} iterations",
                raw_json_size as f64 / 1024.0,
                iterations
            );

            println!(
                "+-------------+------------------+--------------------+------------------+--------------------+--------------------+"
            );
            println!(
                "| Format      | Compress Time    | Decompress Time    | Compressed Size  | Compress MB/s      | Decompress MB/s    |"
            );
            println!(
                "+-------------+------------------+--------------------+------------------+--------------------+--------------------+"
            );

            // Test each format
            let formats = [
                ("Json (raw)", Serialization::Json),
                ("ZstdJson", Serialization::ZstdJson),
            ];

            for (format_name, format) in formats.iter() {
                // Warm up - do a few operations first
                for _ in 0..3 {
                    let compressed = message.to_compressed_bytes(*format).unwrap();
                    let _decoded = Message::from_compressed_bytes(&compressed, *format).unwrap();
                }

                // Measure compression time
                let start = Instant::now();
                let mut compressed_data = Vec::new();
                for _ in 0..iterations {
                    compressed_data = message.to_compressed_bytes(*format).unwrap();
                }
                let compress_duration = start.elapsed();

                // Measure decompression time
                let start = Instant::now();
                for _ in 0..iterations {
                    let _decoded =
                        Message::from_compressed_bytes(&compressed_data, *format).unwrap();
                }
                let decompress_duration = start.elapsed();

                // Calculate metrics
                let avg_compress_time =
                    compress_duration.as_nanos() as f64 / iterations as f64 / 1_000_000.0; // ms
                let avg_decompress_time =
                    decompress_duration.as_nanos() as f64 / iterations as f64 / 1_000_000.0; // ms
                let compressed_size = compressed_data.len();

                // Calculate throughput (MB/s)
                let compress_throughput = (raw_json_size as f64 / 1_048_576.0)
                    / (compress_duration.as_secs_f64() / iterations as f64);
                let decompress_throughput = (raw_json_size as f64 / 1_048_576.0)
                    / (decompress_duration.as_secs_f64() / iterations as f64);

                println!(
                    "| {:<11} | {:>13.2} ms | {:>15.2} ms | {:>13.1} KB | {:>15.1} MB/s | {:>15.1} MB/s |",
                    format_name,
                    avg_compress_time,
                    avg_decompress_time,
                    compressed_size as f64 / 1024.0,
                    compress_throughput,
                    decompress_throughput
                );
            }

            println!(
                "+-------------+------------------+--------------------+------------------+--------------------+--------------------+"
            );
        }

        println!("\nOverall Notes:");
        println!("- Times are averages over multiple iterations");
        println!("- Includes warm-up runs for more accurate timing");
        println!("- ZstdJson provides best compression");
    }

    // Creates a message body with string entries and random content
    fn create_body_with_strings(count: i64) -> Val {
        let mut strings = Vec::new();

        // Add some randomness to string generation
        let adjectives = [
            "big", "small", "fast", "slow", "red", "blue", "green", "yellow", "happy", "sad",
        ];
        let nouns = [
            "cat", "dog", "car", "house", "book", "tree", "phone", "computer", "person", "city",
        ];
        let verbs = [
            "runs", "jumps", "sleeps", "reads", "writes", "talks", "walks", "eats", "drinks",
            "plays",
        ];

        for i in 0..count {
            // Select random words based on the index
            let adj_index = (i as usize * 7 + 3) % adjectives.len();
            let noun_index = (i as usize * 11 + 5) % nouns.len();
            let verb_index = (i as usize * 13 + 7) % verbs.len();

            // Create sentences with some randomized content
            if i % 3 == 0 {
                strings.push(Val::from(format!(
                    "The {} {} {} quickly. Item number {} with additional text for padding.",
                    adjectives[adj_index], nouns[noun_index], verbs[verb_index], i
                )));
            } else if i % 3 == 1 {
                strings.push(Val::from(format!(
                    "Yesterday, a {} {} {} in the garden. Record ID: {}-{}-{}.",
                    adjectives[(adj_index + 1) % adjectives.len()],
                    nouns[(noun_index + 2) % nouns.len()],
                    verbs[verb_index],
                    i,
                    i * 3,
                    i * 7
                )));
            } else {
                strings.push(Val::from(format!(
                    "Have you seen that {} {} that {} like the wind? Reference code: {}/{}/{}.",
                    adjectives[(adj_index + 3) % adjectives.len()],
                    nouns[noun_index],
                    verbs[(verb_index + 1) % verbs.len()],
                    adj_index,
                    noun_index,
                    verb_index
                )));
            }
        }

        Val::Vec(strings)
    }

    // Creates a message body with numeric entries
    fn create_body_with_numbers(count: i64) -> Val {
        let mut numbers = Vec::new();
        for i in 0..count {
            // Mix of integers and decimals
            if i % 2 == 0 {
                numbers.push(Val::Int(i));
            } else {
                numbers.push(crate::val!(i as f64 / 3.0));
            }
        }
        Val::Vec(numbers)
    }

    // Creates a message body with mixed data types
    fn create_body_with_mixed_data(count: i64) -> Val {
        let mut items = Vec::new();
        for i in 0..count {
            match i % 4 {
                0 => items.push(Val::Int(i)),
                1 => items.push(Val::from(format!("String {}", i))),
                2 => items.push(Val::Bool(i % 2 == 0)),
                _ => {
                    // Using Val::from instead of Val::map_from
                    items.push(Val::from([
                        (Val::from("id"), Val::Int(i)),
                        (Val::from("name"), Val::from(format!("Item {}", i))),
                        (Val::from("active"), Val::Bool(true)),
                        (Val::from("score"), crate::val!(i as f64 / 10.0)),
                    ]));
                }
            }
        }
        Val::Vec(items)
    }

    // Creates a message body with repeating pattern data (highly compressible)
    fn create_body_with_repeating_data(count: i64) -> Val {
        let mut items = Vec::new();

        // Create a pattern that repeats
        let pattern = [
            Val::from("This is a repeating string that should compress very well"),
            Val::Int(12345),
            Val::Bool(true),
            crate::val!(12.34),
        ];

        for i in 0..count {
            items.push(pattern[i as usize % pattern.len()].clone());
        }

        Val::Vec(items)
    }

    // Creates a test message with the given body content
    fn create_test_message(label: &str, size: i64, body_content: &Val) -> Message {
        Message {
            id: Uuid::now_v7(),
            head: crate::val!({
                "content-type": "application/json",
                "size": size,
                "label": label
            }),
            body: body_content.clone(),
        }
    }

    // Updated to compare Json and ZstdJson formats
    fn print_compression_stats(label: &str, message: &Message) {
        // Get raw JSON size (uncompressed)
        let raw_json = serde_json::to_vec(&message).unwrap();
        let raw_json_size = raw_json.len();

        // Get ZstdJson format compressed size
        let zstd_json_compressed = message
            .to_compressed_bytes(Serialization::ZstdJson)
            .unwrap();
        let zstd_json_compressed_size = zstd_json_compressed.len();

        // Calculate compression ratios
        let zstd_compression_ratio = if zstd_json_compressed_size > 0 {
            raw_json_size as f64 / zstd_json_compressed_size as f64
        } else {
            0.0
        };

        // Print results
        println!(
            "| {:<9} | {:>13} | {:>17} | {:>16.2}x | {:<9} |",
            label, raw_json_size, zstd_json_compressed_size, zstd_compression_ratio, "ZstdJson"
        );
    }
}
