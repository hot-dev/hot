# aws-eventstream

AWS Event Stream binary protocol parser for streaming AWS services.

## Overview

This package provides a pure Hot implementation for parsing AWS Event Stream messages, the binary framing protocol used by AWS streaming services like:

- **Amazon Bedrock** - Streaming model inference (`ConverseStream`, `InvokeModelWithResponseStream`)
- **Amazon Transcribe** - Real-time transcription streaming
- **Amazon S3 Select** - Streaming query results
- **Amazon Kinesis** - Enhanced fan-out consumers

## Usage

### Parsing a Single Message

```hot
::aws::eventstream use

// Parse a complete event stream message from bytes
result parse-message(message-bytes)

// Check for errors
if(is-error(result)) {
    log("Parse error:", result.error)
} {
    message result.message
    log("Headers:", message.headers)
    log("Payload:", message.payload)
}
```

### Parsing a Stream of Messages

```hot
::aws::eventstream use

// Parse multiple messages from a byte stream
messages parse-stream(stream-bytes)

// Each message has headers and payload
each(messages, (msg) {
    // Common headers: :message-type, :event-type, :content-type
    message-type get(msg.headers, ":message-type")

    match message-type {
        "event" => {
            event-type get(msg.headers, ":event-type")
            // Handle event based on type
        },
        "exception" => {
            // Handle exception
        },
        "error" => {
            // Handle error
        }
    }
})
```

### Bedrock Streaming Example

```hot
::aws::eventstream use
::hot::json use

// After receiving binary stream from Bedrock ConverseStream
events parse-stream(response-bytes)

each(events, (event) {
    event-type get(event.headers, ":event-type")

    match event-type {
        "contentBlockDelta" => {
            data parse(to-str(event.payload))
            delta get(data, "delta")
            text get(delta, "text")
            log(text)
        },
        "messageStop" => {
            log("Stream complete")
        }
    }
})
```

## Event Stream Format

AWS Event Stream uses a binary framing protocol:

```
[total_length:4][headers_length:4][prelude_crc:4][headers:*][payload:*][message_crc:4]
```

- **Prelude** (8 bytes): Total length and headers length as big-endian u32
- **Prelude CRC** (4 bytes): CRC32 checksum of prelude
- **Headers** (variable): Key-value pairs with typed values
- **Payload** (variable): Message body (often JSON)
- **Message CRC** (4 bytes): CRC32 checksum of entire message

### Header Types

| Type | Code | Description |
|------|------|-------------|
| Bool True | 0 | Boolean true |
| Bool False | 1 | Boolean false |
| Byte | 2 | Single byte |
| Short | 3 | 16-bit signed integer |
| Int | 4 | 32-bit signed integer |
| Long | 5 | 64-bit signed integer |
| Bytes | 6 | Variable length bytes |
| String | 7 | UTF-8 string |
| Timestamp | 8 | 64-bit milliseconds since epoch |
| UUID | 9 | 16-byte UUID |

## API Reference

### Types

- `Message` - Parsed event stream message with headers and payload
- `ParseResult` - Result of parsing (success or error)

### Functions

- `parse-message(bytes)` - Parse a single event stream message
- `parse-stream(bytes)` - Parse all messages from a byte stream
- `get-header(message, name)` - Get a header value by name
- `is-event(message)` - Check if message is an event
- `is-exception(message)` - Check if message is an exception
- `is-error(message)` - Check if message is an error

## Dependencies

- `hot.dev/hot-std` - Core Hot standard library (bytes, bit, crc32)

## License

Apache-2.0
