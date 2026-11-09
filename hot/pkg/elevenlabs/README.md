# elevenlabs

ElevenLabs API bindings for Hot: text-to-speech (full audio, character timestamps, and streaming), Scribe speech-to-text, sound effects, voices, models, generation history, account quota, and webhook verification. It covers both ends of a voice agent: what the agent says and what it hears.

## Installation

Add this to the `deps` in your `hot.hot` file:

```hot
"hot.dev/elevenlabs": "0.1.2"
```

## Setup

| Context Variable | Description |
|---|---|
| `elevenlabs.api.key` | API key from [elevenlabs.io/app/settings/api-keys](https://elevenlabs.io/app/settings/api-keys) (sent as the `xi-api-key` header) |
| `elevenlabs.webhook.secret` | For `::elevenlabs::webhooks/verify-request`: the signing secret shown when you create a webhook under Settings → Webhooks |

The free tier includes a monthly character quota that is enough for development.

The API key can also be passed per call; see [Per-call credentials](#per-call-credentials).

## Quick start

```hot
::tts ::elevenlabs::tts
::stt ::elevenlabs::stt

// Text to speech: audio Bytes, mp3 44.1kHz by default
audio ::tts/text-to-speech(::elevenlabs/GEORGE_VOICE_ID, ::tts/TextToSpeechRequest({
  text: "Hello from Hot!",
  model_id: "eleven_multilingual_v2"
}))
::hot::file/write-file-bytes("hello.mp3", audio)

// Speech to text: transcribe the bytes back
heard ::stt/transcript-text(::stt/speech-to-text(audio))
```

## Usage

### Text to speech

```hot
::tts ::elevenlabs::tts

request ::tts/TextToSpeechRequest({
  text: "Your order has shipped.",
  model_id: "eleven_flash_v2_5",
  voice_settings: {stability: 0.5, similarity_boost: 0.75}
})

// Output format as a third argument: telephony, PCM, opus...
phone ::tts/text-to-speech(::elevenlabs/GEORGE_VOICE_ID, request, "ulaw_8000")

// Character-level timing for captions or lip sync; audio comes back decoded
timed ::tts/text-to-speech-with-timestamps(::elevenlabs/GEORGE_VOICE_ID, request)
::hot::file/write-file-bytes("shipped.mp3", timed.audio)
captions ::tts/alignment-words(timed.alignment)   // [{word, start, end}]

// Streaming: raw audio chunks as they are generated
stream ::tts/text-to-speech-stream(::elevenlabs/GEORGE_VOICE_ID, request)
chunks collect(stream.body)
```

`TextToSpeechRequest` fields: `text` (required), `model_id` (`eleven_multilingual_v2` default, `eleven_flash_v2_5`, `eleven_v3`, ...), `voice_settings`, `language_code`, `pronunciation_dictionary_locators`, `seed`, `previous_text`/`next_text`, `previous_request_ids`/`next_request_ids`, `apply_text_normalization`, `apply_language_text_normalization`. Unset fields are not sent.

### Speech to text (Scribe)

```hot
::stt ::elevenlabs::stt

audio ::hot::file/read-file-bytes("meeting.mp3")
out ::stt/speech-to-text(audio, ::stt/SpeechToTextRequest({
  diarize: true,
  keyterms: ["Hot Dev", "Scribe"],
  tag_audio_events: false
}), "meeting.mp3")

::stt/transcript-text(out)   // full text
::stt/words(out)             // [{text, start, end, speaker_id, ...}], spacing dropped

// Hosted file instead of an upload
remote ::stt/speech-to-text-url("https://example.com/podcast.mp3")
```

The default model is `scribe_v2`. The upload is a `multipart/form-data` body built as `Bytes`, so binary audio goes up untouched. With `webhook: true` the call returns `{request_id, transcription_id}` straight away and the transcript is delivered to your speech-to-text webhook; fetch or delete stored transcripts with `get-transcript` / `delete-transcript`.

### Sound effects

```hot
sfx ::elevenlabs::sound/sound-effect("Heavy wooden door creaking open", {duration_seconds: 3, prompt_influence: 0.5})
```

Options: `duration_seconds` (0.5 to 30), `prompt_influence` (0 to 1), `loop`, `model_id`, `output_format`.

### Voices, models, account

```hot
page ::elevenlabs::voices/list-voices({page_size: 50, category: "premade"})
names map(page.voices, (v) { v.name })
voice ::elevenlabs::voices/get-voice(::elevenlabs/GEORGE_VOICE_ID)
settings ::elevenlabs::voices/get-voice-settings(::elevenlabs/GEORGE_VOICE_ID)

models ::elevenlabs::models/list-models()

plan ::elevenlabs::user/get-subscription()
remaining sub(plan.character_limit, plan.character_count)
```

### History

```hot
::history ::elevenlabs::history

recent ::history/list-history({page_size: 5, source: "TTS"})
item first(recent.history)
audio ::history/get-history-audio(item.history_item_id)
::history/delete-history-item(item.history_item_id)
```

Page with `start_after_history_item_id: recent.last_history_item_id` while `has_more` is true.

## Webhooks

ElevenLabs signs each delivery with an `ElevenLabs-Signature: t=<unix seconds>,v0=<hex>` header, an HMAC-SHA256 of `<t>.<raw body>`. `verify-request` checks it over the raw body in constant time and rejects deliveries more than 30 minutes from now (pass a tolerance in seconds as the third argument to change that, or 0 to skip it). It fails closed when the secret is unset or empty.

```hot
elevenlabs-webhook
meta {webhook: {service: "elevenlabs", path: "/elevenlabs/webhook"}}
fn (request) {
  if(not(::elevenlabs::webhooks/verify-request(request)),
    ::hot::http/HttpResponse({status: 401, body: "invalid signature"}),
    ::hot::http/HttpResponse({status: 200, body: {received: request.body.type}}))
}
```

## Errors

Non-2xx responses return an Err carrying `::elevenlabs::api/HttpError` with `status`, `headers`, `body`, and a readable `message` built from ElevenLabs' `detail` (for example `ElevenLabs API error (HTTP 401): invalid_api_key: Invalid API key`). A missing API key returns an Err before any request is sent. `::elevenlabs::api/json` and `::elevenlabs::api/bytes` reach any other endpoint.

## Per-call credentials

Every function that calls ElevenLabs takes an optional trailing `opts` Map, so one run can act for many tenants and parallel flows don't share credentials. Credentials in `opts` are masked in call records, and nothing from `opts` is written to ctx.

| Key | Effect |
|---|---|
| `api-key` | The API key for this call, instead of `elevenlabs.api.key` |
| `base-url` | Another API root, such as a proxy. Needs an `api-key` in the same `opts`; `elevenlabs.api.key` is never sent to a per-call URL |

`opts` always comes last. Where a function already takes `options` or an output format, pass `{}` or `null` for those.

```hot
::tts ::elevenlabs::tts

tenant {api-key: "tenant-b-key"}
audio ::tts/text-to-speech(::elevenlabs/GEORGE_VOICE_ID, ::tts/TextToSpeechRequest({text: "Hello"}), null, tenant)
voices ::elevenlabs::voices/list-voices({page_size: 50}, tenant)
quota ::elevenlabs::user/get-subscription(tenant)
```

An `api-key` that is present but blank or null is an error naming the key, never a fallback to `elevenlabs.api.key`. `::elevenlabs::api/resolve(opts)` is the one place this is decided. `::elevenlabs::webhooks/verify-request` takes an explicit secret too, and masks it the same way.

## Notes and limits

- Voice cloning (instant and professional), voice design, dubbing, speech-to-speech, the music API, and Conversational AI agents are out of scope.
- Realtime websocket TTS and STT are not covered; `text-to-speech-stream` streams over HTTP.
- History holds generations made by this API key's account. `delete-history-item` is permanent.
- Integration tests (`hot test -p pkg-integration-elevenlabs`) need `ELEVENLABS_API_KEY`. They list voices and models and generate one short phrase, which uses a few characters of quota and adds one item to the account's history.

## License

Apache-2.0 - see [LICENSE](LICENSE)
