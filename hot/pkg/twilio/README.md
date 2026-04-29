# twilio

Twilio REST API bindings for Hot. Send SMS/MMS, make voice calls, manage phone numbers, and verify webhooks.

## Installation

Add this to the `deps` in your `hot.hot` file:

```hot
"hot.dev/twilio": "1.1.0"
```

## Configuration

Two context variables are required:

- `twilio.account.sid` — Your Twilio Account SID (starts with `AC`)
- `twilio.auth.token` — Your Twilio Auth Token

Set them via the Hot app context variables.

For webhook verification, the auth token is also used (no additional config needed).

## Usage

### Send an SMS

```hot
::messages ::twilio::messages

response ::messages/create-message(::messages/CreateMessageRequest({
  To: "+15558675310",
  From: "+15017122661",
  Body: "Hello from Hot!"
}))
response.sid     // => "SM..."
response.status  // => "queued"
```

### Send an MMS

```hot
::messages ::twilio::messages

response ::messages/create-message(::messages/CreateMessageRequest({
  To: "+15558675310",
  From: "+15017122661",
  Body: "Check out this image!",
  MediaUrl: ["https://example.com/image.jpg"]
}))
```

### Send a WhatsApp Message

```hot
::messages ::twilio::messages

response ::messages/create-message(::messages/CreateMessageRequest({
  To: "whatsapp:+15558675310",
  From: "whatsapp:+15017122661",
  Body: "Hello from Hot via WhatsApp!"
}))
```

### Make a Phone Call

```hot
::calls ::twilio::calls

response ::calls/create-call(::calls/CreateCallRequest({
  To: "+15558675310",
  From: "+15017122661",
  Twiml: "<Response><Say>Hello from Hot!</Say></Response>"
}))
```

### Check Message Status

```hot
::messages ::twilio::messages

response ::messages/get-message(::messages/GetMessageRequest({
  sid: "SM1234567890abcdef1234567890abcdef"
}))
response.status  // => "delivered"
```

### List Your Phone Numbers

```hot
::accounts ::twilio::accounts

response ::accounts/get-incoming-phone-numbers(::accounts/GetIncomingPhoneNumbersRequest({}))
```

### Verify a Webhook

```hot
::webhooks ::twilio::webhooks

is-valid ::webhooks/verify-request(request)
```

## API Base URL

`https://api.twilio.com/2010-04-01`

## Modules

| Module | Description |
|--------|-------------|
| `::twilio::messages` | Send SMS/MMS/WhatsApp, get, list, update, delete messages |
| `::twilio::calls` | Make calls, get, list, update, delete call records |
| `::twilio::accounts` | Account info, phone number management |
| `::twilio::webhooks` | HMAC-SHA1 webhook signature verification |
| `::twilio::api` | Low-level authenticated HTTP client (Basic Auth + form encoding) |
| `::twilio::core` | Shared configuration (BASE_URL) |

## Integration Tests

Integration tests run against the live Twilio API and will send real SMS messages (which incur charges).

### 1. Get Your Twilio Credentials

1. Sign up at [twilio.com](https://www.twilio.com/) (free trial accounts work)
2. From the [Twilio Console](https://console.twilio.com/), copy your **Account SID** and **Auth Token**
3. Get a Twilio phone number (trial accounts include one free number)

### 2. Set Context Variables

Set these via the Hot app context variables:

```
twilio.account.sid=ACxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
twilio.auth.token=your-auth-token
```

### 3. Set Environment Variables

Add these to your `.env` file:

```
TWILIO_TEST_FROM=+15017122661
TWILIO_TEST_TO=+15558675310
```

- `TWILIO_TEST_FROM` — Your Twilio phone number (the one you purchased/received)
- `TWILIO_TEST_TO` — A phone number to send test messages to (on trial accounts, this must be a verified number)

### 4. Trial Account Limitations

If you're using a Twilio trial account:
- You can only send messages **to verified phone numbers** (verify at Console > Phone Numbers > Verified Caller IDs)
- Outbound messages are prefixed with "Sent from your Twilio trial account"
- You can only make calls **to verified phone numbers**

### 5. Run the Tests

```bash
hot test hot/pkg/twilio/integration-test/
```

### What the Tests Do

| Test File | What It Tests | Side Effects |
|-----------|--------------|--------------|
| `messages.hot` | Send SMS, fetch message by SID, list messages | Sends a real SMS (costs ~$0.0079) |
| `accounts.hot` | Get account info, list phone numbers | Read-only |
| `calls.hot` | List call records | Read-only |

## Documentation

- [Twilio REST API Documentation](https://www.twilio.com/docs/usage/api)
- [Hot Package Documentation](https://hot.dev/pkg/hot.dev/twilio)

## License

Apache-2.0 - see [LICENSE](LICENSE)
