# elevenlabs

ElevenLabs API bindings for Hot: text-to-speech (including streaming), voices, models, and account quota. The speech-output half of a voice agent — pair with `whisper` (speech-to-text), `telegram`/`twilio` (delivery), and `hot-ai-agent` (the brain).

## Setup

Context variable `elevenlabs.api.key` (from [elevenlabs.io](https://elevenlabs.io/app/settings/api-keys)).

## Usage

```hot
::tts ::elevenlabs::tts

// Text to speech — returns audio Bytes (mp3 by default)
audio ::tts/text-to-speech(::elevenlabs/GEORGE_VOICE_ID, ::tts/TextToSpeechRequest({
  text: "Hello from Hot!",
  model_id: "eleven_multilingual_v2"
}))
::hot::file/write("hot://out/hello.mp3", audio)

// Telephony format for Twilio phone agents
audio ::tts/text-to-speech(voice-id, request, "ulaw_8000")

// Streaming (raw audio chunks as they generate)
stream ::tts/text-to-speech-stream(voice-id, request)

// Voices and models
::elevenlabs::voices/list-voices()
::elevenlabs::models/list-models()
::elevenlabs::models/get-user()  // quota
```

## License

Apache-2.0 - see [LICENSE](LICENSE)
