# deepgram

Deepgram bindings: prerecorded speech-to-text (`transcribe-url` / `transcribe-audio`, nova-3 with smart formatting) and Aura text-to-speech (`speak`). Pull the text with `transcript(response)`. Context variable: `deepgram.api.key`.

```hot
out ::deepgram/transcribe-url(recording-url, {diarize: true})
::deepgram/transcript(out)
```

## License

Apache-2.0 - see [LICENSE](LICENSE)
