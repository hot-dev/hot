# Hot Standard Library Reference

## Core vs Namespaced Functions

Hot has two kinds of stdlib functions:

1. **Core** (`meta {core: true}`) — Auto-imported, available everywhere without namespace prefix. Just call them directly: `add(1, 2)`, `map(items, fn)`, `Str(42)`.
2. **Namespaced** — Require the full `::namespace/function` path, or a namespace/var alias to use.

### Importing Namespaced Functions

```hot
// Namespace alias (most common) — create a short alias for a namespace
::http ::hot::http
response ::http/get("https://api.example.com")

// Var import — import a single function into your namespace
sha256 ::hot::hash/sha256
hash sha256("data")

// Full qualified path — no alias needed, just verbose
hash ::hot::hash/sha256("data")
```

### Making Your Own Functions Core

Mark any function with `meta {core: true}` to make it available project-wide without imports:

```hot
::myapp::utils ns

format-currency
meta {core: true, doc: "Format a number as currency"}
fn (amount: Dec): Str { `$${amount}` }
```

---

## Core Functions (auto-imported, no prefix needed)

These functions have `meta {core: true}` and are available everywhere without namespace qualification.

### Math

Math functions accept both `Int` and `Dec`. When mixing types, the result is `Dec`.

<!-- hot-check: skip -->
```hot
add fn (x: Int | Dec, y: Int | Dec): Int | Dec          // Addition
sub fn (x: Int | Dec, y: Int | Dec): Int | Dec          // Subtraction
mul fn (x: Int | Dec, y: Int | Dec): Int | Dec          // Multiplication
div fn (x: Int | Dec, y: Int | Dec): Int | Dec          // Division
mod fn (x: Int | Dec, y: Int | Dec): Int | Dec          // Remainder
pow fn (base: Int | Dec, exp: Int | Dec): Int | Dec     // Exponentiation
abs fn (x: Int | Dec): Int | Dec                        // Absolute value (not in source, but in runtime)
min fn (x: Int | Dec, y: Int | Dec): Int | Dec          // Minimum
max fn (x: Int | Dec, y: Int | Dec): Int | Dec          // Maximum
round fn (x: Int | Dec): Int                            // Round to nearest integer
floor fn (x: Int | Dec): Int                            // Round down
ceil fn (x: Int | Dec): Int                             // Round up
is-zero fn (x: Int | Dec): Bool                         // true if x is 0
is-pos fn (x: Int | Dec): Bool                          // true if x > 0
is-neg fn (x: Int | Dec): Bool                          // true if x < 0
rand fn (): Dec                                         // Random decimal 0-1
rand fn (n: Int): Dec                                   // Random decimal 0-n
rand-int fn (): Int                                     // Random integer
rand-int fn (n: Int): Int                               // Random integer 0 to n-1
```

### Comparison

<!-- hot-check: skip -->
```hot
eq fn (a: Any, b: Any): Bool                            // Equality
ne fn (a: Any, b: Any): Bool                            // Not equal
lt fn (a: Int | Dec, b: Int | Dec): Bool                // Less than
gt fn (a: Int | Dec, b: Int | Dec): Bool                // Greater than
lte fn (a: Int | Dec, b: Int | Dec): Bool               // Less than or equal
gte fn (a: Int | Dec, b: Int | Dec): Bool               // Greater than or equal
```

### Logic

<!-- hot-check: skip -->
```hot
is-truthy fn (value: Result): Bool                      // Not false, null, or Err
not fn (x: Result): Bool                                // Negate truthiness
and fn (x: Result, lazy ...rest: Result): Any           // First falsy value, or last (short-circuits)
or fn (x: Result, lazy ...rest: Result): Any            // First truthy value, or last (short-circuits)
if fn cond (pred: Result, lazy then: Any): Any          // If pred, evaluate then; else null
if fn cond (pred: Result, lazy then: Any, lazy else: Any): Any  // If pred, evaluate then; else evaluate else
```

### Collections (Eager)

Most collection functions work on `Vec`, `Map`, and `Str`.

<!-- hot-check: skip -->
```hot
// Transform
map fn (coll: Str | Vec | Map, func: Fn): Str | Vec | Map           // Apply func to each element
pmap fn (coll: Str | Vec | Map, func: Fn): Str | Vec | Map          // Parallel map
map-indexed fn (coll: Str | Vec | Map, func: Fn): Str | Vec | Map   // Map with (index, element)
filter fn (coll: Str | Vec | Map, predicate: Fn): Str | Vec | Map   // Keep matching elements
remove fn (coll: Str | Vec | Map, predicate: Fn): Str | Vec | Map   // Remove matching elements
reduce fn (coll: Str | Vec | Map, reducer: Fn, initial: Any): Any   // Fold to single value
mapcat fn (coll: Str | Vec | Map, f: Fn): Str | Vec | Map           // Map then flatten

// Access
first fn (coll: Str | Vec | Map): Any                  // First element
last fn (coll: Str | Vec | Map): Any                   // Last element
rest fn (coll: Str | Vec | Map): Str | Vec | Map       // All but first
butlast fn (coll: Str | Vec | Map): Str | Vec | Map    // All but last
get fn (coll: Map | Vec | Str | Bytes, key: Any): Any             // Get by key/index (null if missing)
get fn (coll: Map | Vec | Str | Bytes, key: Any, not-found: Any): Any  // Get with default
length fn (coll: Str | Vec | Map | Bytes): Int         // Number of elements
is-empty fn (coll: Str | Vec | Map): Bool              // true if no elements

// Build & combine
concat fn (a: Vec, b: Vec, ...rest: Vec): Vec           // Concatenate vectors
concat fn (a: Str, b: Str, ...rest: Str): Str           // Concatenate strings
concat fn (a: Bytes, b: Bytes, ...rest: Bytes): Bytes   // Concatenate bytes
flatten fn (coll: Vec): Vec                             // Flatten nested vectors at every depth; strings and maps stay whole
merge fn (a: Map, b: Map): Map                          // Deep merge maps (b overrides a)
assoc fn (map: Map, key: Any, value: Any): Map          // Set key-value (immutable)
assoc-some fn (map: Map, key: Any, value: Any): Map     // Like assoc, but no-op if value is null
update fn (map: Map, key: Any, func: Fn): Map           // Apply func to value at key
delete fn (coll: Str | Vec | Map, key-or-index: Str | Int): Str | Vec | Map  // Remove by key/index
zipmap fn (a: Vec, b: Vec): Map                         // Create map from key vec + value vec

// Slice & partition
slice fn (coll: Str | Vec | Map | Bytes, start: Int): Str | Vec | Map | Bytes         // From start to end
slice fn (coll: Str | Vec | Map | Bytes, start: Int, end: Int): Str | Vec | Map | Bytes  // From start to end (exclusive)
partition fn (coll: Str | Vec | Map, size: Int): Str | Vec | Map      // Split into groups of size
partition-by fn (coll: Str | Vec | Map, func: Fn): Str | Vec | Map   // Split when func value changes

// Order & uniqueness
sort fn (coll: Str | Vec | Map): Str | Vec | Map                     // Sort ascending
sort-by fn (coll: Str | Vec | Map, func: Fn): Str | Vec | Map        // Sort by key function
reverse fn (coll: Str | Vec | Map): Str | Vec | Map                  // Reverse order
distinct fn (coll: Str | Vec | Map): Str | Vec | Map                 // Remove duplicates
shuffle fn (coll: Str | Vec | Map): Str | Vec | Map                  // Randomize order

// Query
some fn (coll: Str | Vec | Map, predicate: Fn): Bool    // true if any element matches
all fn (coll: Str | Vec | Map, predicate: Fn): Bool     // true if all elements match
find-first fn (coll: Vec, predicate: Fn): Any           // First match or null (short-circuits)

// Map-specific
keys fn (coll: Str | Vec | Map): Str | Vec | Map        // Keys of map (or indices of vec/str)
vals fn (coll: Str | Vec | Map): Str | Vec | Map        // Values of map (or elements of vec)
map-keys fn (m: Map, func: Fn): Map                     // Transform all keys
map-vals fn (m: Map, func: Fn): Map                     // Transform all values
filter-keys fn (m: Map, predicate: Fn): Map             // Keep entries where key matches
filter-vals fn (m: Map, predicate: Fn): Map             // Keep entries where value matches
index-by fn (coll: Vec, func: Fn): Map                  // Index collection by key function
group-by fn (coll: Vec, func: Fn): Map                  // Group elements by key function
frequencies fn (coll: Vec): Map                          // Count occurrences of each element

// Deep path operations
get-in fn (coll: Map | Vec, path: Vec): Any             // Get nested value by path
get-in fn (coll: Map | Vec, path: Vec, default: Any): Any  // Get nested value with default
assoc-in fn (coll: Map | Vec, path: Vec, value: Any): Map | Vec  // Set nested value by path
update-in fn (coll: Map | Vec, path: Vec, func: Fn): Map | Vec   // Apply func to nested value

// Interleave & interpose
interleave fn (a: Str | Vec | Map, b: Str | Vec | Map): Str | Vec | Map  // Alternate elements
interpose fn (coll: Str | Vec | Map, separator: Any): Str | Vec | Map    // Insert separator between elements

// Range
range fn (end: Int | Dec): Vec                                           // [0, 1, ..., end-1]
range fn (start: Int | Dec, end: Int | Dec): Vec                         // [start, ..., end-1]
range fn (start: Int | Dec, end: Int | Dec, step: Int | Dec): Vec        // With step

// Tree walking
walk fn (inner: Fn, outer: Fn, form: Any): Any           // Walk structure: inner on children, outer on result
prewalk fn (f: Fn, form: Any): Any                       // Apply f to each node before children
postwalk fn (f: Fn, form: Any): Any                      // Apply f to each node after children
postwalk-replace fn (replacement-map: Map, form: Any): Any  // Replace values via post-order walk
```

### Strings

<!-- hot-check: skip -->
```hot
split fn (s: Str, delim: Str): Vec<Str>                 // Split by delimiter
join fn (values: Vec<Str>, delim: Str): Str              // Join with delimiter
trim fn (s: Str): Str                                    // Trim whitespace from both ends
trim-start fn (s: Str): Str                              // Trim whitespace from start
trim-end fn (s: Str): Str                                // Trim whitespace from end
replace fn (s: Str, from: Str, to: Str): Str             // Replace first occurrence
uppercase fn (s: Str): Str                               // Convert to uppercase
lowercase fn (s: Str): Str                               // Convert to lowercase
contains fn (s: Str, substring: Str): Bool               // true if s contains substring
starts-with fn (s: Str, prefix: Str): Bool               // true if s starts with prefix
ends-with fn (s: Str, suffix: Str): Bool                 // true if s ends with suffix
is-blank fn (s: Str | Null): Bool                        // true if null, empty, or whitespace only
pad-start fn (s: Str, width: Int): Str                   // Pad left with spaces
pad-start fn (s: Str, width: Int, pad: Str): Str         // Pad left with custom char
pad-end fn (s: Str, width: Int): Str                     // Pad right with spaces
pad-end fn (s: Str, width: Int, pad: Str): Str           // Pad right with custom char
```

### Results

<!-- hot-check: skip -->
```hot
Result enum { Ok(Any), Err(Any) }                        // Success/error union type
ok fn (value: Any): Result                               // Wrap in Result.Ok
err fn (value: Any): Result                              // Wrap in Result.Err
is-ok fn (result: Result): Bool                         // true if result is Ok
is-err fn (result: Result): Bool                        // true if result is Err
is-result fn (value: Result): Bool                       // true if value is a Result
if-ok fn (result: Result, handler): Any                 // Ok → apply fn/use value; Err → pass through
if-err fn (result: Result, handler): Any                // Err → apply fn/use value; Ok → pass through
```

### Type Checks

<!-- hot-check: skip -->
```hot
is-type fn (val: Result, type-ref): Bool                // Check if val conforms to type-ref
is-null fn (value: Any): Bool                            // true if null
is-some fn (value: Any): Bool                            // true if not null
is-str fn (value: Any): Bool                             // true if Str
is-int fn (value: Any): Bool                             // true if Int
is-dec fn (value: Any): Bool                             // true if Dec
is-bool fn (value: Any): Bool                            // true if Bool
is-vec fn (value: Any): Bool                             // true if Vec
is-map fn (value: Any): Bool                             // true if Map
is-fn fn (value: Any): Bool                              // true if Fn
is-bytes fn (value: Any): Bool                           // true if Bytes
is-byte fn (value: Any): Bool                            // true if Byte
```

### Type Constructors & Coercion

<!-- hot-check: skip -->
```hot
Str type fn (value: Any): Str                            // Coerce to string
Int type fn (value: Int | Dec): Int                      // Coerce to integer (truncates)
Dec type fn (value: Int | Dec): Dec                      // Coerce to decimal
Bool type fn (value: Any): Bool                          // Coerce to boolean (truthiness)
Vec type fn (value: Any): Vec<Any>                       // Coerce to vector
Map type fn (value: Any): Map<Any, Any>                  // Coerce to map
Byte type fn (value: Byte): Byte                         // Single byte (0-255)
Bytes type fn (value: Bytes): Bytes                      // Byte array
Null type fn (): Null                                    // Null value
Any type fn (value: Any): Any                            // Identity (no constraint)
Fn type fn (value: Str | Fn): Fn                         // Function reference
Var type fn (value: Str | Var): Var                      // Variable reference
Namespace type fn (value: Str | Namespace): Namespace    // Namespace reference
untype fn (form: Any): Any                               // Strip internal type metadata from typed structures
```

### JSON

<!-- hot-check: skip -->
```hot
from-json fn (s: Str): Any                               // Parse JSON string
to-json fn (value: Any): Str                             // Serialize to JSON string
```

### UUID (core)

<!-- hot-check: skip -->
```hot
Uuid fn (): Str                                          // Generate new UUID v4
is-uuid fn (value: Any): Bool                            // true if value is a valid UUID
```

### Random (core)

<!-- hot-check: skip -->
```hot
random-bytes fn (n: Int): Bytes                          // Generate n random bytes
random-string fn (n: Int): Str                           // Generate random alphanumeric string of length n
secure-compare fn (a: Str, b: Str): Bool                 // Constant-time string comparison
```

### XML (core)

<!-- hot-check: skip -->
```hot
from-xml fn (s: Str): Map                                // Parse XML string to map
to-xml fn (value: Map): Str                              // Convert map to XML string
child fn (node: Map, name: Str): Map                     // Get first child element by name
children fn (node: Map, name: Str): Vec<Map>             // Get all child elements by name
text fn (node: Map): Str                                 // Get text content of element
attr fn (node: Map, name: Str): Str                      // Get attribute value
at fn (node: Map, path: Str): Any                        // Navigate XML path
```

### File I/O (core)

<!-- hot-check: skip -->
```hot
read-file fn (path: Str): Str                            // Read file as UTF-8 string
read-file-bytes fn (path: Str): Bytes                    // Read file as bytes
write-file fn (path: Str, content: Str): Bool            // Write string to file
write-file-bytes fn (path: Str, content: Bytes): Bool    // Write bytes to file
delete-file fn (path: Str): Bool                         // Delete a file
file-exists fn (path: Str): Bool                         // Check if file exists
file-info fn (path: Str): Map                            // Get file metadata
list-files fn (path: Str): Vec<Str>                      // List files in directory
```

### Events (core)

<!-- hot-check: skip -->
```hot
send fn (event-name: Str, data: Any): Map                // Send event for async processing
```

### Predicates (core)

<!-- hot-check: skip -->
```hot
between fn (x: Int | Dec, low: Int | Dec, high: Int | Dec): Bool  // Inclusive range check: low <= x <= high
in fn (value: Any, coll: Vec): Bool                      // Membership test: true if value is in coll
```

### Utility (core)

<!-- hot-check: skip -->
```hot
tap fn (value: Result): Any                              // Print value to stderr, return unchanged
tap fn (value: Result, label: Str): Any                  // Print "label: value" to stderr, return unchanged
print fn (val: Any): Str                                 // Print to stdout (no newline)
println fn (val: Any): Str                               // Print to stdout with newline
assert fn (actual: Any): Bool                            // Assert truthy
assert fn (actual: Any, msg: Str): Bool                  // Assert truthy with message
assert-eq fn (expected: Any, actual: Any): Bool          // Assert equality
assert-eq fn (expected: Any, actual: Any, msg: Str): Bool  // Assert equality with message
fail fn (msg: Str): Failure                              // Halt with error
fail fn (msg: Str, data: Any): Failure                   // Halt with error and data
version fn (): Str                                       // Hot runtime version
```

---

## Namespaced Functions (require prefix or alias)

These functions are NOT auto-imported. You must use the full qualified path (`::hot::http/get(...)`) or create a namespace/var alias first.

### ::hot::env

Environment variables.

<!-- hot-check: skip -->
```hot
::env ::hot::env

get fn (name: Str, default-value: Str): Str              // Get env var with default
get-all fn (): Map<Str, Str>                             // Get all env vars
```

### ::hot::ctx

Execution context for secrets and configuration.

<!-- hot-check: skip -->
```hot
::ctx ::hot::ctx

get fn (key: Str | Var): Any                             // Get context value
set fn (key: Str | Var, value: Any): Any                 // Set a single value
set fn (ctx-map: Map): Any                               // Set multiple values from map
set-secret fn (key: Str | Var, value: Any): Any          // Set a single secret value
set-secret fn (ctx-map: Map): Any                        // Set multiple secret values from map
```

### ::hot::http

HTTP client functions.

<!-- hot-check: skip -->
```hot
::http ::hot::http

get fn (url: Str): HttpResponse
post fn (url: Str, body: Any): HttpResponse
put fn (url: Str, body: Any): HttpResponse
patch fn (url: Str, body: Any): HttpResponse
delete fn (url: Str): HttpResponse
request fn (request: HttpRequest): HttpResponse         // Transport errors (DNS, TLS, timeouts) return Result.Err
request fn (method: HttpMethod, url: Str, headers: Map<Str, Str>, body: Any): HttpResponse
request-stream fn (request: HttpRequest): StreamingHttpResponse
request-stream fn (request: HttpRequest, format: StreamFormat): StreamingHttpResponse
request-stream fn (method: HttpMethod, url: Str, headers: Map<Str, Str>, body: Any): StreamingHttpResponse
request-stream fn (method: HttpMethod, url: Str, headers: Map<Str, Str>, body: Any, format: StreamFormat): StreamingHttpResponse
get-stream fn (url: Str): StreamingHttpResponse
get-stream fn (url: Str, format: StreamFormat): StreamingHttpResponse
post-stream fn (url: Str, body: Any): StreamingHttpResponse
post-stream fn (url: Str, body: Any, headers: Map<Str, Str>): StreamingHttpResponse
post-stream fn (url: Str, body: Any, headers: Map<Str, Str>, format: StreamFormat): StreamingHttpResponse
is-ok-response fn (response: HttpResponse): Bool         // true if status is 2xx
```

Response structure: `{status: Int, headers: Map<Str, Str>, body: Any}`

Use `HttpRequest` whenever you need headers, raw body control, or parity with
webhook handlers:

```hot
HttpRequest ::hot::http/HttpRequest

response ::http/request(HttpRequest({
    method: "POST",
    url: "https://api.example.com/users",
    headers: {Content-Type: "application/json"},
    body: {name: "Alice"},
}))
```

### ::hot::uri

URI parsing, building, encoding, and validation.

<!-- hot-check: skip -->
```hot
::uri ::hot::uri

Uri type { scheme: Str, userinfo: Str?, host: Str?, port: Int?, path: Str, query: Str?, fragment: Str? }

encode fn (value: Str): Str                              // Percent-encode a URI component (RFC 3986)
decode fn (value: Str): Str                              // Percent-decode a string
encode-query fn (params: Map): Str                       // Map to query string (application/x-www-form-urlencoded)
decode-query fn (query: Str): Map                        // Query string to Map (strips leading ?)
parse fn (uri: Str): Uri                                 // Parse string into Uri type
build fn (parts: Map): Str                               // Build URI string from components map
join fn (base: Str, part: Str): Str                      // Join/resolve a path segment against base
join fn (base: Str, part1: Str, part2: Str): Str         // Two segments
join fn (base: Str, part1: Str, part2: Str, part3: Str): Str // Three segments (the most join takes)
is-valid fn (uri: Str): Bool                             // Check if valid URI
```

`Uri -> Str` coercion is supported: a `Uri` value auto-coerces to `Str` when passed to functions expecting strings (e.g. `::http/get`).

`build` accepts a `query` field as either a `Str` or a `Map` (which gets form-encoded automatically).

### ::hot::base64

<!-- hot-check: skip -->
```hot
::b64 ::hot::base64

encode fn (val: Str | Bytes): Str                        // Encode to base64
decode fn (s: Str): Bytes                                // Decode from base64
encode-url fn (val: Str | Bytes): Str                    // URL-safe base64 encode (no padding)
decode-url fn (s: Str): Bytes                            // URL-safe base64 decode
is-valid fn (s: Str): Bool                               // Check if valid base64
```

### ::hot::regex

<!-- hot-check: skip -->
```hot
::re ::hot::regex

first-match fn (value: Str, pattern: Str): Vec<Str>      // First match or null
find fn (value: Str, pattern: Str): Str                  // Find first match
find-all fn (value: Str, pattern: Str): Vec<Str>         // Find all matches
replace fn (value: Str, pattern: Str, replacement: Str): Str      // Replace first match
replace-all fn (value: Str, pattern: Str, replacement: Str): Str  // Replace all matches
split fn (value: Str, pattern: Str): Vec<Str>            // Split by pattern
is-match fn (value: Str, pattern: Str): Bool             // true if pattern matches
capture fn (value: Str, pattern: Str): Str               // First captured group
capture-all fn (value: Str, pattern: Str): Vec<Str>      // All captured groups
escape fn (value: Str): Str                              // Escape special regex chars
```

### ::hot::hash

<!-- hot-check: skip -->
```hot
::hash ::hot::hash

sha256 fn (data: Str | Bytes): Str                       // SHA-256 (64-char hex)
blake3 fn (data: Str | Bytes): Str                       // BLAKE3 hash
sha384 fn (data: Str | Bytes): Str                       // SHA-384 (96-char hex)
sha512 fn (data: Str | Bytes): Str                       // SHA-512 (128-char hex)
```

### ::hot::hmac

<!-- hot-check: skip -->
```hot
::hmac ::hot::hmac

hmac-sha512 fn (key: Str | Bytes, data: Str | Bytes): Str  // HMAC-SHA512 (128-char hex)
hmac-sha1 fn (key: Str | Bytes, data: Str | Bytes): Str    // HMAC-SHA1 (legacy)
```

### ::hot::time

Types: `Instant`, `PlainDate`, `PlainTime`, `PlainDateTime`, `ZonedDateTime`, `Duration`

<!-- hot-check: skip -->
```hot
::time ::hot::time

// Current time
now fn (): Instant                                        // Current instant (UTC)
now-zoned fn (timezone: Str): ZonedDateTime               // Current time in timezone

// Parsing
parse fn (date-string: Str): PlainDateTime | PlainDate | PlainTime // By the string's shape

// Instant accessors
epoch-millis fn (instant: Instant): Int                   // Milliseconds since Unix epoch
epoch-nanos fn (instant: Instant): Int                    // Nanoseconds since Unix epoch

// Component extraction
year fn (date: PlainDate | PlainDateTime): Int            // Extract year
month fn (date: PlainDate | PlainDateTime): Int           // Extract month (1-12)
day fn (date: PlainDate | PlainDateTime): Int             // Extract day (1-31)
hour fn (time: PlainTime | PlainDateTime): Int            // Extract hour (0-23)
minute fn (time: PlainTime | PlainDateTime): Int          // Extract minute (0-59)
second fn (time: PlainTime | PlainDateTime): Int          // Extract second (0-59)

// Day boundaries
start-of-day fn (date: PlainDate | PlainDateTime): PlainDateTime  // 00:00:00
end-of-day fn (date: PlainDate | PlainDateTime): PlainDateTime    // 23:59:59

// Arithmetic
add fn (temporal: PlainDateTime, duration: Duration): PlainDateTime
subtract fn (temporal: PlainDateTime, duration: Duration): PlainDateTime
until fn (start: PlainDateTime, end: PlainDateTime): Duration
since fn (start: PlainDateTime, end: PlainDateTime): Duration

// Duration constructors
years fn (n: Any): Duration
months fn (n: Any): Duration
weeks fn (n: Any): Duration
days fn (n: Any): Duration
hours fn (n: Any): Duration
minutes fn (n: Any): Duration
seconds fn (n: Any): Duration

// Timezone operations
with-timezone fn (zdt: ZonedDateTime, tz: Str): ZonedDateTime   // Convert to new timezone
to-plain-date-time fn (zdt: ZonedDateTime): PlainDateTime       // Strip timezone
to-plain-date fn (zdt: ZonedDateTime): PlainDate                // Extract date
to-plain-time fn (zdt: ZonedDateTime): PlainTime                // Extract time
to-instant fn (zdt: ZonedDateTime): Instant                     // Extract exact moment

// Formatting (pattern-based, English names)
// Tokens: YYYY YY MMMM MMM MM M DD D dddd ddd HH H hh h mm ss SSS A a Z z X
// Escape literals with [brackets]
format fn (temporal: Any, pattern: Str): Str
```

**ZonedDateTime** constructors:
```hot
ZonedDateTime("2026-02-17T10:30:00-06:00[America/Chicago]")      // From IXDTF string
ZonedDateTime(::time/now(), "America/Chicago")                     // From Instant + timezone
ZonedDateTime(PlainDateTime(2026, 3, 8, 2, 30, 0), "US/Central") // From PlainDateTime + timezone
```

**Format examples:**
```hot
::time/format(PlainDate(2026, 2, 17), "MMMM D, YYYY")         // "February 17, 2026"
::time/format(PlainDateTime(2026, 2, 17, 14, 30, 0), "h:mm A") // "2:30 PM"
```

### ::hot::exec

Execution control. `fail`, `cancel`, and `exit` are core (auto-imported), so
call them without a prefix. They live in `::hot::exec`, not `::hot::run`.

<!-- hot-check: skip -->
```hot
fail fn (err: Any): Failure                              // Halt with error
fail fn (msg: Str, err: Any): Failure                    // Halt with error and data
cancel fn (msg: Str): Cancellation                       // Cancel current run
cancel fn (msg: Str, data: Any): Cancellation            // Cancel current run with data
exit fn (code: Int): Any                                 // Exit with status code
```

### ::hot::run

Run information. `info`, `is-inline-run`, and `is-local-dev` are also core
(auto-imported).

<!-- hot-check: skip -->
```hot
info fn (): RunInfo                                      // Information about the current run
is-inline-run fn (): Bool                                // true for run, eval, repl, and test runs
is-local-dev fn (): Bool                                 // true under `hot dev` and `hot run` defaults
```

`info()` returns a `RunInfo` with nested `run`, `stream`, `build`, `project`,
`env`, `user`, and `org` fields. Common run fields: `id`, `type`
(`"call"|"event"|"schedule"|"run"|"eval"|"repl"`), `status`, `retry-attempt`,
`max-retries`. Useful in retry-aware handlers:

```hot
i info()
if(gt(i.run.retry-attempt, 0),
    println(`retry ${i.run.retry-attempt} of ${i.run.max-retries}`),
    null)
```

### ::hot::lang

Language-level helpers. Rely on normal `Result` propagation — there is no
halt-catching helper (`try` / `try-call` were removed in Hot 2.6.0).

<!-- hot-check: skip -->
```hot
call fn (f: Fn, args: Vec<Any>): Any                     // Force domain Err by default
call fn (f: Fn, args: Vec<Any>, on-err: OnErr): Any      // Use OnErr.Preserve to return domain Err as value
call fn (f: Fn): Any                                     // Same as call(f, [])
```

> Prefer Result propagation for normal domain errors. Use `OnErr.Preserve` on
> eligible APIs (`map`, `pmap`, `map-indexed`, `mapcat`, `call`) when you want
> to keep a domain `Err` as a value:

```hot
// Fan-out where one failing item must not abort the run.
results map(items, process-item, OnErr.Preserve)

failures filter(results, is-err)
```

### ::hot::iter

Lazy iterator functions. `Iter`, `next`, `collect`, `for-each`, `take` are also core.

<!-- hot-check: skip -->
```hot
Iter fn (coll: Vec): Iter                                // Create iterator from collection
next fn (iter: Iter): Next                               // Get next value ({value, done})
collect fn (iter: Iter): Vec                             // Collect remaining to vector
for-each fn (iter: Iter, fn: Fn): Any                    // Execute fn for each element
take fn (iter: Iter, n: Int): Vec                        // Take first n elements
range fn (start: Int, end: Int): Iter                    // Range iterator
range fn (start: Int, end: Int, step: Int): Iter         // Range iterator with step
```

### ::hot::bytes

<!-- hot-check: skip -->
```hot
::bytes ::hot::bytes

to-int fn (bytes: Bytes): Int                            // Bytes to signed int (big-endian)
to-int fn (bytes: Bytes, endian: Str): Int               // Bytes to signed int ("big" or "little")
to-uint fn (bytes: Bytes): Int                           // Bytes to unsigned int (big-endian)
to-uint fn (bytes: Bytes, endian: Str): Int              // Bytes to unsigned int with endianness
from-int fn (value: Int, size: Int): Bytes               // Int to bytes (big-endian)
from-int fn (value: Int, size: Int, endian: Str): Bytes  // Int to bytes with endianness
crc32 fn (data: Bytes | Str | Vec): Int                  // CRC32 checksum
to-vec fn (bytes: Bytes): Vec<Int>                       // Bytes to vector of ints
```

### ::hot::bit

<!-- hot-check: skip -->
```hot
::bit ::hot::bit

and fn (a: Int | Byte, b: Int | Byte): Int | Byte       // Bitwise AND
or fn (a: Int | Byte, b: Int | Byte): Int | Byte        // Bitwise OR
xor fn (a: Int | Byte, b: Int | Byte): Int | Byte       // Bitwise XOR
not fn (a: Int | Byte): Int | Byte                       // Bitwise NOT
shift-left fn (a: Int | Byte, n: Int): Int | Byte       // Shift left by n bits
shift-right fn (a: Int | Byte, n: Int): Int | Byte      // Arithmetic shift right
```

### ::hot::hex

<!-- hot-check: skip -->
```hot
::hex ::hot::hex

is-valid fn (s: Str): Bool                               // true if valid hexadecimal
```

### ::hot::meta

<!-- hot-check: skip -->
```hot
::meta ::hot::meta

get fn (lazy var: Any): Any                              // Get metadata attached to a variable
```

### ::hot::task

Long-running task execution. Start tasks from runs, send/receive messages, checkpoint state.

<!-- hot-check: skip -->
```hot
::task ::hot::task

// Types
Failure type { msg: Str, err: Any }                      // Failed task execution
Cancellation type { msg: Str, data: Any }                // Cancelled task execution
TaskInfo type { id: Str, stream: Stream, origin-run: Run } // Returned by start
TaskOptions type { timeout: Int?, type: Str?, retry: Int | Map? }
TaskResult type { id: Str, status: Str, exit-code: Int?, ... } // Returned by await

// Task lifecycle
start fn (task-fn: Fn): TaskInfo                         // Start a task with no args
start fn (task-fn: Fn, args: Any): TaskInfo              // Start with args
start fn (task-fn: Fn, args: Any, options: TaskOptions): TaskInfo // Start with options
cancel fn (task-id: Str): Bool                           // Cancel a queued/running task
await fn (task-id: Str): TaskResult                      // Wait only when this Hot execution depends on completion

// Messaging (code tasks only)
send fn (task-id: Str, data: Any): Bool                  // Send data to a running task
receive fn (): Any                                       // Receive next message (blocks)

// State persistence (code tasks only, inside a task)
checkpoint fn (data: Any): Bool                          // Save state for retry recovery
restore fn (): Any?                                      // Get last checkpoint (null if none)
restore fn (task-id: Str): Any?                          // Get checkpoint for a specific task
```

### ::hot::store

Persistent ordered maps with optional embedding and semantic search.
Available in CLI, event handlers, and tasks.

<!-- hot-check: skip -->
```hot
::store ::hot::store

// Types
Map type { name: Str, embedding: EmbeddingOptions? }     // Named persistent map
Embedding type { model: Str?, field: Str?, text-search: Bool? }

// CRUD
put fn (store: Map, key: Str, value: Any): Any           // Set entry, replacing existing value
put-if-missing fn (store: Map, key: Str, value: Any): Bool // Atomically create; false if key exists
put-if fn (store: Map, key: Str, expected: Any, value: Any): Bool // Compare-and-set; null expects a missing key (since 3.0.1)
increment fn (store: Map, key: Str): Int                 // Atomically add 1; a missing key counts from 0 (since 3.0.1)
increment fn (store: Map, key: Str, by: Int): Int        // Atomically add by; returns the new value
get fn (store: Map, key: Str): Any?                      // Get by key (null if missing)
get fn (store: Map, key: Str, default: Any): Any         // Get with default
delete fn (store: Map, key: Str): Bool                   // Remove entry
keys fn (store: Map): Vec<Str>                           // All keys
keys fn (store: Map, opts: Map): Vec<Str>                // Keys with {prefix (since 3.0.1), limit, offset}
vals fn (store: Map): Vec<Any>                           // All values
length fn (store: Map): Int                              // Entry count
is-empty fn (store: Map): Bool                           // True if no entries
first fn (store: Map): Any?                              // First entry
last fn (store: Map): Any?                               // Last entry

// Bulk operations
put-many fn (store: Map, entries: Map): Int              // Insert multiple entries; returns how many
merge fn (store: Map, entries: Map): Int                 // Alias for put-many
list fn (store: Map): Vec                                // List entries (default limit 1000)
list fn (store: Map, opts: Map): Vec                     // List with {limit, offset, prefix}
clear fn (store: Map): Bool                              // Remove all entries
destroy fn (store: Map): Bool                            // Delete the store itself

// Search (requires embedding config)
search fn (store: Map, query: Str): Vec                  // Semantic search (default limit 10)
search fn (store: Map, query: Str, opts: Map): Vec       // Search with {limit, mode, min-score}

// Iteration
filter fn (store: Map, predicate: Fn): Vec               // Filter entries
find-first fn (store: Map, predicate: Fn): Any?          // First matching entry
some fn (store: Map, predicate: Fn): Bool                // Any entry matches?
all fn (store: Map, predicate: Fn): Bool                 // All entries match?
reduce fn (store: Map, reducer: Fn, initial: Any): Any   // Reduce over entries
slice fn (store: Map, start: Int): Vec                   // Entries from offset
slice fn (store: Map, start: Int, end: Int): Vec         // Entries in range
```

Handlers can run at the same time, so `get` then `put` on a shared key can
lose another handler's write. Use `put-if-missing` to create an entry once,
`increment` for counters, and a bounded `put-if` retry loop to change a value.
The computation must be pure, since it can run more than once. Give the loop
an attempt budget, so a bug (an `expected` that isn't what `get` returned)
fails with an error instead of retrying forever:

```hot
::store ::hot::store

cases ::store/Map({name: "cases"})

add-answer fn (case-id: Str, answer-id: Str, answer: Any, attempts: Int): Map {
    current ::store/get(cases, case-id)
    next assoc(if(is-null(current), {}, current), answer-id, answer)
    wrote ::store/put-if(cases, case-id, current, next)
    cond {
        wrote => { next }
        lte(attempts, 1) => { err(`case ${case-id} kept changing`) }
        => { add-answer(case-id, answer-id, answer, sub(attempts, 1)) }
    }
}

add-answer("case-1", "answer-1", {score: 4}, 20)
```

`list` and `keys` with `prefix` read only the Str keys that start with it,
from an index; `filter` reads every entry.

---

## Iterators vs Collections

Hot has two approaches for working with sequences:

**Eager (Collections)**: Process entire collection at once. Good for small/medium data.

```hot
// All elements processed immediately
doubled map([1, 2, 3], (x) { mul(x, 2) })  // [2, 4, 6]
```

**Lazy (Iterators)**: Process on-demand. Good for large data, streaming, or when you don't need all results.

```hot
// Nothing processed until consumed
it Iter([1, 2, 3, 4, 5])
next(it)        // Next({value: 1, done: false})
next(it)        // Next({value: 2, done: false})
collect(it)     // [3, 4, 5] (remaining)

// Efficient: doesn't allocate 10000 items
first-five take(range(1, 10001), 5)  // [1, 2, 3, 4, 5]

// Streaming HTTP
response ::http/request-stream("GET", url, {}, "", "sse")
for-each(response.body, (event) { process(event) })
```

## Events

Send events for async processing:

```hot
// Send event
send("user:created", {id: "123", email: "a@b.com"})

// Handle event (in another function)
on-user-created meta {on-event: "user:created"}
fn (event) {
    send-welcome-email(event.data.email)
}
```
