# deepgram

Deepgram bindings for Hot: pre-recorded speech-to-text (Nova-3, from a URL or raw audio bytes), asynchronous transcription with callbacks, Aura-2 and Flux text-to-speech, the public model list, and helpers that turn a `/v1/listen` response into transcript text, words, speaker turns, and paragraphs.

## Installation

Add this to the `deps` in your `hot.hot` file:

```hot
"hot.dev/deepgram": "0.1.0"
```

## Setup

| Context Variable | Description |
|---|---|
| `deepgram.api.key` | API key from the [Deepgram console](https://console.deepgram.com/) (sent as `Authorization: Token <key>`) |
| `deepgram.callback.secret` | Optional. Shared secret sent with async requests and checked by `::deepgram::callbacks/verify-callback`. Use a hex value (`openssl rand -hex 32`) |

New Deepgram accounts come with free credit, enough to run the integration suite many times.

Both variables can also be passed per call; see [Per-call credentials](#per-call-credentials).

## Quick start

```hot
::dg ::deepgram

// Transcribe a hosted file (nova-3 + smart formatting by default)
out ::dg/transcribe-url("https://static.deepgram.com/examples/Bueller-Life-moves-pretty-fast.wav")
text ::dg/transcript(out)

// Speak, then transcribe the audio bytes back
audio ::dg/speak("Your build is green.")
heard ::dg/transcript(::dg/transcribe-audio(audio))
```

## Usage

### Speech to text

```hot
::dg ::deepgram

// Options map to Deepgram query params. Vec values repeat the key
// (keyterm=Bueller&keyterm=Ferris), which is what Deepgram expects.
out ::dg/transcribe-url("https://static.deepgram.com/examples/Bueller-Life-moves-pretty-fast.wav", {
  diarize_model: "latest",
  utterances: true,
  paragraphs: true,
  keyterm: ["Bueller", "Ferris"]
})

::dg/transcript(out)            // best transcript, first channel
::dg/words(out)                 // [{word, start, end, confidence, speaker, ...}]
::dg/utterances(out)            // speaker turns (needs utterances: true)
::dg/format-utterances(out)     // "[Speaker 0] ...\n[Speaker 1] ..."
::dg/paragraphs(out)            // needs paragraphs: true
::dg/paragraph-transcript(out)  // paragraph-formatted text
::dg/request-id(out)

// Raw audio. Containerized formats are detected by Deepgram; headerless
// PCM needs encoding and sample_rate.
call-audio ::hot::file/read-file-bytes("call.mp3")
call ::dg/transcribe-audio(call-audio, {language: "multi"})
pcm ::hot::file/read-file-bytes("call.raw")
raw ::dg/transcribe-audio(pcm, {encoding: "linear16", sample_rate: 16000})
```

Common options: `model` (default `nova-3`), `language` (`"multi"` for code-switching), `detect_language`, `diarize_model` (replaces the deprecated `diarize`), `utterances`, `utt_split`, `paragraphs`, `smart_format` (default `true`; pass `null` to drop it), `punctuate`, `numerals`, `filler_words`, `keyterm` (Nova-3), `keywords` (older models), `search`, `replace`, `redact`, `summarize`, `topics`, `intents`, `sentiment`, `multichannel`, `tag`, and `extra` (a Map becomes `extra=key:value` pairs).

### Asynchronous transcription (callbacks)

Long recordings can return right away: Deepgram answers with a `request_id` and later POSTs the full response to your callback URL.

```hot
::dg ::deepgram

request-id ::dg/transcribe-url-async("https://static.deepgram.com/examples/Bueller-Life-moves-pretty-fast.wav",
  "https://example.com/hooks/deepgram", {utterances: true, extra: {job: "42"}})
```

When `deepgram.callback.secret` is set, the async functions add `extra=callback_secret:<secret>`; Deepgram echoes it back in `metadata.extra`, and the handler checks it:

```hot
deepgram-callback
meta {webhook: {service: "deepgram", path: "/deepgram/callback"}}
fn (request) {
  if(not(::deepgram::callbacks/verify-callback(request)),
    ::hot::http/HttpResponse({status: 401, body: "invalid callback"}),
    ::deepgram/format-utterances(request.body))
}
```

Deepgram does not sign callbacks. Its documented options are a shared secret in `extra` (above) or Basic Auth credentials embedded in the callback URL (`https://user:pass@host/path`), checked with `::deepgram::callbacks/verify-basic-auth(request, user, pass)`. The `dg-token` header carries the identifier of the API key that made the request. It is not a secret and is not sent on every callback, so `verify-dg-token(request, key-id)` is only a supplementary check. Callbacks only go to ports 80, 443, 8080, and 8443, and a non-2xx answer is retried up to 10 times, 30 seconds apart.

### Text to speech

```hot
::dg ::deepgram

// Aura-2 (v1/speak): mp3 Bytes by default
audio ::dg/speak("Your build is green.", {model: "aura-2-orion-en"})
::hot::file/write-file-bytes("build.mp3", audio)

// Telephony: 8 kHz mu-law without a container
phone ::dg/speak("Press one to continue.", {encoding: "mulaw", sample_rate: 8000, container: "none"})

// Flux (v2/speak): model is required
flux ::dg/speak-flux("Welcome back.", "flux-alexis-en", {expressivity: 1})
```

### Models

```hot
models ::deepgram/list-models()
voices map(models.tts, (m) { m.canonical_name })
```

## Errors

Non-2xx responses return an Err carrying `::deepgram::api/HttpError` with `status`, `headers`, `body`, and a readable `message` built from Deepgram's error body (for example `Deepgram API error (HTTP 401): INVALID_AUTH: Invalid credentials.`). A missing API key returns an Err before any request is sent.

## Per-call credentials

Every function that calls Deepgram takes an optional trailing `opts` Map, so one run can act for many tenants and parallel flows don't share credentials. Credentials in `opts` are masked in call records, and nothing from `opts` is written to ctx.

| Key | Effect |
|---|---|
| `api-key` | The API key for this call, instead of `deepgram.api.key` |
| `base-url` | Another API root, such as a proxy. Needs an `api-key` in the same `opts`; context credentials are never sent to a per-call URL |
| `callback-secret` | The shared secret the async functions send with this call, instead of `deepgram.callback.secret` |

`opts` always comes last. Where a function already takes `options`, pass `{}` for those.

```hot
tenant {api-key: "tenant-b-key"}
out ::deepgram/transcribe-url("https://static.deepgram.com/examples/Bueller-Life-moves-pretty-fast.wav", {}, tenant)
audio ::deepgram/speak("Your build is green.", {}, tenant)
models ::deepgram/list-models(tenant)
```

An `api-key` or `callback-secret` that is present but blank or null is an error naming the key, never a fallback to context. `::deepgram::api/resolve(opts)` is the one place this is decided. The verifiers in `::deepgram::callbacks` take an explicit secret too, and mask it the same way.

## Notes and limits

- Live streaming over websockets (`wss://api.deepgram.com/v1/listen`, Flux streaming STT, and the Voice Agent API) is not covered. For near-real-time flows, transcribe short chunks with `transcribe-audio`.
- `speak` and `speak-flux` are synchronous; TTS callbacks are not wrapped.
- Text Intelligence (`/v1/read`) and the management API (projects, keys, usage) are not wrapped; `::deepgram::api/json` reaches any endpoint.
- Integration tests (`hot test -p pkg-integration-deepgram`) need `DEEPGRAM_API_KEY`; they spend a few seconds of transcription credit and change nothing on the account.

## License

Apache-2.0 - see [LICENSE](LICENSE)
