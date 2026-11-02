# twilio

Twilio API bindings for Hot: send SMS, MMS, and WhatsApp messages, place and control voice calls, send and check one-time passcodes with Verify, look up phone numbers, download recordings and inbound media, answer webhooks with TwiML builders, and verify webhook signatures. Every call authenticates with an Auth Token or an API key, from context or from an explicit client.

## Install

Add this to the `deps` in your `hot.hot` file:

```hot
"hot.dev/twilio": "1.1.0"
```

## Setup

| Context Variable | Required | Description |
|---|---|---|
| `twilio.account.sid` | yes, unless every call passes credentials | Account SID (`AC...`). Every 2010-04-01 resource URL is scoped to it. |
| `twilio.api.key.sid` | one of these | API Key SID (`SK...`). With `twilio.api.key.secret`, preferred over the Auth Token when both are set. |
| `twilio.api.key.secret` | credential forms | The API key's secret. |
| `twilio.auth.token` | | Auth Token. Also the key Twilio signs webhooks with, so `::twilio::webhooks/verify-request` needs it even when calls use an API key. |
| `twilio.verify.service.sid` | no | Default Verify Service (`VA...`) for `::twilio::verify`. |

Twilio recommends API keys (Console > Account > API keys & tokens): they can be revoked one at a time and never expose the account's own credentials.

To act on another account (a subaccount, or a customer's account in a multi-tenant app), build a client and pass it as the first argument of any resource function:

```hot
::api ::twilio::api
::messages ::twilio::messages

sub ::api/api-key-client("AC1111111111111111111111111111111", "SK2222222222222222222222222222222", "key-secret")
page ::messages/list-messages(sub, ::messages/ListMessagesRequest({PageSize: 20}))
```

`::api/auth-token-client(account-sid, auth-token)` does the same with an Auth Token, and `::api/client-from-ctx()` is the client the short forms use.

## Per-call credentials

Every function that calls Twilio also takes an optional trailing `opts` Map, so one run can act for many accounts and parallel flows don't share credentials. Nothing from `opts` is written to ctx. Secrets in `opts` or in a `Client` are masked in call records.

| Key | Effect |
|---|---|
| `account-sid` | The account for this call. Needs credentials in the same `opts`; context credentials are never sent to a per-call account |
| `auth-token` | The account's Auth Token. Needs `account-sid` |
| `api-key-sid`, `api-key-secret` | An API key. Needs `account-sid`, and wins over `auth-token` |

`opts` always comes last.

```hot
::messages ::twilio::messages

tenant {account-sid: "AC1111111111111111111111111111111", api-key-sid: "SK2222222222222222222222222222222", api-key-secret: "key-secret"}
page ::messages/list-messages(::messages/ListMessagesRequest({PageSize: 20}), tenant)
everything ::messages/list-all-messages(::messages/ListMessagesRequest({PageSize: 100}), 300, tenant)
```

With credentials in `opts`, context variables are ignored for that call. A credential key that is present but blank or null is an error naming the key, never a fallback to context. `::twilio::api/resolve` is the one place credentials are decided, whether they come from `opts`, a `Client` or context.

## Quick start

```hot
::messages ::twilio::messages
::calls ::twilio::calls
::verify ::twilio::verify
::twiml ::twilio::twiml

// SMS, and MMS with up to 10 public media URLs
sms ::messages/create-message(::messages/CreateMessageRequest({
  To: "+15558675310",
  From: "+15017122661",
  Body: "Your order has shipped."
}))
mms ::messages/create-message(::messages/CreateMessageRequest({
  To: "+15558675310",
  From: "+15017122661",
  Body: "Here's the receipt",
  MediaUrl: ["https://example.com/receipt.png"]
}))

// A call that speaks, then reports progress to a status webhook
call ::calls/create-call(::calls/CreateCallRequest({
  To: "+15558675310",
  From: "+15017122661",
  Twiml: ::twiml/voice-response([::twiml/say("Your table is ready.")]),
  StatusCallback: "https://myapp.hot.dev/twilio/call-status",
  StatusCallbackEvent: ["initiated", "answered", "completed"]
}))

// One-time passcode: send, then check what the user typed
::verify/start-verification(::verify/StartVerificationRequest({To: "+15558675310", Channel: "sms"}))
check ::verify/check-verification(::verify/CheckVerificationRequest({To: "+15558675310", Code: "123456"}))
signed-in eq(check.status, "approved")
```

## Functions

**Messages** (`::twilio::messages`)
- `create-message` sends SMS, MMS (`MediaUrl`), WhatsApp (`whatsapp:` numbers), or content templates (`ContentSid`); schedule with `SendAt` and `ScheduleType: "fixed"` from a Messaging Service.
- `get-message`, `update-message` (redact with `Body: ""`, cancel a scheduled message with `Status: "canceled"`), `delete-message`.
- `list-messages` returns one page; `list-all-messages(request, limit)` follows page links. `DateSentBefore`/`DateSentAfter` map to Twilio's `DateSent<`/`DateSent>`.
- `list-media` and `get-media-content` (returns `Bytes`) for inbound MMS.

**Calls** (`::twilio::calls`)
- `create-call` with `Url`, `Twiml`, or `ApplicationSid`; answering machine detection, recording, status callbacks.
- `get-call`, `update-call` (hang up with `Status: "completed"`, or redirect with new `Url`/`Twiml`), `delete-call`.
- `list-calls`, `list-all-calls`.

**Recordings** (`::twilio::recordings`): `list-recordings` (filter by `CallSid`), `get-recording`, `download-recording` (`Bytes`, mp3 or wav), `delete-recording`.

**Verify** (`::twilio::verify`): `start-verification`, `check-verification`, `get-verification`, `update-verification` (cancel or approve); `create-service`, `get-service`, `list-services`, `delete-service`.

**Lookup** (`::twilio::lookups`): `lookup-phone-number` validates and formats a number for free; `Fields` adds paid packages such as `line_type_intelligence`, `caller_name`, or `sms_pumping_risk`.

**Account and numbers** (`::twilio::accounts`): `get-account`, `update-account`, `get-incoming-phone-numbers`, `get-incoming-phone-number`, `update-incoming-phone-number` (point a number's SMS and voice webhooks at Hot), `delete-incoming-phone-number`.

**TwiML** (`::twilio::twiml`): `voice-response` and `messaging-response` documents; voice verbs `say`, `play`, `pause`, `gather`, `dial` (with nouns `number`, `client`, `sip`, `conference`, `queue`), `enqueue`, `record`, `hangup`, `reject`, `redirect`; messaging `message` (with `body`, `media`); `element` for anything else; `http-response` for the webhook reply. Text and attribute values are XML-escaped.

**Low level** (`::twilio::api`): `request`, `json`, `form-encode`, `query-string`, `next-page`, `collect-pages`, `resolve`, and the client constructors.

### Paging

List functions return one page. `::twilio::api/next-page(page)` fetches the next one (null on the last page), using the `next_page_uri` link that carries Twilio's required `PageToken`. `list-all-messages` and `list-all-calls` collect up to a limit:

```hot
::messages ::twilio::messages

recent ::messages/list-all-messages(::messages/ListMessagesRequest({To: "+15558675310", PageSize: 100}), 300)
```

Page links are only followed to `twilio.com` hosts, so credentials never go anywhere else.

## Webhooks

Point a number's webhooks at Hot (`update-incoming-phone-number` or the Console), verify the `X-Twilio-Signature`, and answer with TwiML:

```hot
::webhooks ::twilio::webhooks
::twiml ::twilio::twiml

HttpRequest ::hot::http/HttpRequest
HttpResponse ::hot::http/HttpResponse

on-sms
meta {webhook: {service: "twilio", path: "/sms"}}
fn (request: HttpRequest): HttpResponse {
  cond {
    not(::webhooks/verify-request(request)) => { HttpResponse({status: 403, body: "invalid signature"}) }
    => {
      text or(request.body.Body, "")
      ::twiml/http-response(::twiml/messaging-response([::twiml/message(`You said: ${text}`)]))
    }
  }
}

on-voice
meta {webhook: {service: "twilio", path: "/voice"}}
fn (request: HttpRequest): HttpResponse {
  cond {
    not(::webhooks/verify-request(request)) => { HttpResponse({status: 403, body: "invalid signature"}) }
    => {
      ::twiml/http-response(::twiml/voice-response([
        ::twiml/gather({input: "dtmf speech", numDigits: 1, action: "/twilio/menu"}, [
          ::twiml/say("Press 1 or say sales. Press 2 or say support.", {voice: "Polly.Joanna"})
        ]),
        ::twiml/say("Sorry, I didn't get that."),
        ::twiml/redirect("/twilio/voice")
      ]))
    }
  }
}
```

`verify-request` checks the HMAC-SHA1 signature over the URL Twilio called (`request.original-url`) plus the sorted form parameters, or, for JSON callbacks, over the URL with its `bodySHA256` parameter and the body's SHA-256. It uses `twilio.auth.token` (the 2-arity form takes the token explicitly), fails closed when the token, signature, or URL is missing, and compares in constant time. If a proxy rewrites the URL, pass the exact URL configured at Twilio as the third argument. Twilio does not sign a timestamp, so there is no replay window to enforce.

## Notes and limits

- Request bodies are form-encoded; unset fields are omitted, and Vec fields (`MediaUrl`, `StatusCallbackEvent`) become repeated parameters.
- Non-2xx responses return `err(HttpError)` with Twilio's `{code, message, more_info, status}` body. Verify answers 404 when a verification is already approved, canceled, expired, or out of attempts.
- Twilio's [Test Credentials](https://www.twilio.com/docs/iam/test-credentials) (a separate Account SID and Auth Token) create messages and calls without charge or delivery, using magic numbers such as `+15005550006` as `From`. They support only creating messages, calls, and phone numbers, plus Lookup; every other request (fetching or listing resources, Verify) answers 403.
- Trial accounts can only message and call verified numbers, and prefix SMS with a trial notice.
- `Fields` packages in Lookup are billed per request; with no `Fields`, Lookup is free.

## Integration tests

The suite in `integration-test/` runs against the live API when credentials are set and otherwise skips every test:

| Environment Variable | Purpose |
|---|---|
| `TWILIO_ACCOUNT_SID` | Account SID |
| `TWILIO_AUTH_TOKEN` | Auth Token (or use the API key pair) |
| `TWILIO_API_KEY_SID`, `TWILIO_API_KEY_SECRET` | API key |
| `TWILIO_TEST_FROM`, `TWILIO_TEST_TO` | Optional: your Twilio number and a number you own; enables the test that sends one real SMS (about $0.008) |

```bash
hot test --project pkg-integration-twilio
```

Account, number, call, and message listing tests are read-only. Without `TWILIO_TEST_FROM`/`TWILIO_TEST_TO`, nothing is sent.

## Documentation

- [Twilio REST API](https://www.twilio.com/docs/usage/api), [Verify](https://www.twilio.com/docs/verify/api), [Lookup v2](https://www.twilio.com/docs/lookup/v2-api), [TwiML](https://www.twilio.com/docs/voice/twiml)
- [Webhook security](https://www.twilio.com/docs/usage/webhooks/webhooks-security)
- [Hot package documentation](https://hot.dev/pkg/hot.dev/twilio)

## License

Apache-2.0 - see [LICENSE](LICENSE)
