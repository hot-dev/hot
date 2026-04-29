# whatsapp

WhatsApp Business Cloud API bindings for Hot. Send messages, templates, interactive messages, manage media, and verify webhooks -- directly through Meta's API, no Twilio required.

## Installation

Add this to the `deps` in your `hot.hot` file:

```hot
"hot.dev/whatsapp": "1.1.0"
```

## Configuration

Set the `whatsapp.access.token` context variable to your Meta access token via the Hot app.

For webhook verification, also set:
- `whatsapp.app.secret` -- your Meta App Secret (for HMAC-SHA256 payload verification)
- `whatsapp.verify.token` -- your chosen verification token (for webhook subscription challenges)

## Usage

### Send a Text Message

```hot
::messages ::whatsapp::messages

response ::messages/send-text(::messages/SendTextRequest({
  phone_number_id: "109876543210",
  to: "15551234567",
  text: "Hello from Hot!"
}))
response.messages[0].id  // => "wamid.HBg..."
```

### Send an Image

```hot
::messages ::whatsapp::messages

response ::messages/send-image(::messages/SendImageRequest({
  phone_number_id: "109876543210",
  to: "15551234567",
  image_url: "https://example.com/photo.jpg",
  caption: "Check this out!"
}))
```

### Send a Template Message

```hot
::messages ::whatsapp::messages

response ::messages/send-template(::messages/SendTemplateRequest({
  phone_number_id: "109876543210",
  to: "15551234567",
  template_name: "hello_world",
  language_code: "en_US"
}))
```

### Send Reply Buttons

```hot
::messages ::whatsapp::messages

response ::messages/send-buttons(::messages/SendButtonsRequest({
  phone_number_id: "109876543210",
  to: "15551234567",
  body_text: "How can we help?",
  buttons: [
    {type: "reply", reply: {id: "sales", title: "Sales"}},
    {type: "reply", reply: {id: "support", title: "Support"}},
    {type: "reply", reply: {id: "info", title: "More Info"}}
  ]
}))
```

### Send a List Message

```hot
::messages ::whatsapp::messages

response ::messages/send-list(::messages/SendListRequest({
  phone_number_id: "109876543210",
  to: "15551234567",
  body_text: "Choose a product:",
  button_text: "View Options",
  sections: [
    {title: "Electronics", rows: [
      {id: "phone", title: "Phone", description: "$999"},
      {id: "laptop", title: "Laptop", description: "$1499"}
    ]}
  ]
}))
```

### React to a Message

```hot
::messages ::whatsapp::messages

::messages/send-reaction(::messages/SendReactionRequest({
  phone_number_id: "109876543210",
  to: "15551234567",
  message_id: "wamid.HBg...",
  emoji: "👍"
}))
```

### Mark a Message as Read

```hot
::messages ::whatsapp::messages

::messages/mark-read(::messages/MarkReadRequest({
  phone_number_id: "109876543210",
  message_id: "wamid.HBg..."
}))
```

### Manage Templates

```hot
::templates ::whatsapp::templates

// List templates
response ::templates/list-templates(::templates/ListTemplatesRequest({
  waba_id: "102938475610"
}))

// Create a template (requires Meta approval)
response ::templates/create-template(::templates/CreateTemplateRequest({
  waba_id: "102938475610",
  name: "order_update",
  language: "en_US",
  category: "UTILITY",
  components: [
    {type: "BODY", text: "Your order {{1}} has been shipped."}
  ]
}))
```

### Business Profile

```hot
::business ::whatsapp::business

// Get profile
response ::business/get-business-profile("109876543210")

// Update profile
::business/update-business-profile(::business/UpdateBusinessProfileRequest({
  phone_number_id: "109876543210",
  about: "Hot Dev - Build faster",
  websites: ["https://hot.dev"]
}))
```

### Verify a Webhook Request

```hot
::webhooks ::whatsapp::webhooks

// Verify payload signature (HMAC-SHA256)
is-valid ::webhooks/verify-request(request)

// Verify subscription challenge (GET request from Meta)
challenge ::webhooks/verify-challenge(request.query)
```

## API Base URL

`https://graph.facebook.com/v21.0`

## Modules

| Module | Description |
|--------|-------------|
| `::whatsapp::messages` | Text, media, location, contacts, templates, interactive (buttons/lists), reactions, read receipts |
| `::whatsapp::media` | Get, download, and delete media files |
| `::whatsapp::templates` | Create, list, edit, and delete message templates |
| `::whatsapp::webhooks` | HMAC-SHA256 signature verification and webhook challenge verification |
| `::whatsapp::business` | Business profile management, phone number info |
| `::whatsapp::api` | Low-level Graph API client |
| `::whatsapp::core` | Shared configuration (BASE_URL) |

## Integration Tests

Integration tests run against the live WhatsApp Business Cloud API.

### 1. Create a Meta App

1. Go to [Meta for Developers](https://developers.facebook.com/) and log in
2. Click **My Apps** > **Create App**
3. Select **Business** type, give it a name, and create it
4. In the App Dashboard, find **WhatsApp** and click **Set Up**
5. This creates a WhatsApp Business Account (WABA) with a test phone number

### 2. Get Your Credentials

From the **WhatsApp > API Setup** page in the App Dashboard:

- **Temporary Access Token** -- displayed on the page (valid 24 hours). For production, create a [System User token](https://developers.facebook.com/docs/whatsapp/business-management-api/get-started#system-user-access-tokens) with `whatsapp_business_messaging` and `whatsapp_business_management` permissions.
- **Phone Number ID** -- shown under "From" on the API Setup page (e.g., `109876543210`)
- **WhatsApp Business Account ID** -- shown in Business Settings (e.g., `102938475610`)
- **App Secret** -- found under **App Settings > Basic** in the App Dashboard

### 3. Add a Test Recipient

On the API Setup page under **To**, click **Manage phone number list** to add your personal phone number as a test recipient. Meta's test environment only allows sending to pre-registered numbers.

### 4. Set Context Variables

```
whatsapp.access.token=EAAx...your_access_token
whatsapp.app.secret=your_app_secret
```

### 5. Set Environment Variables

Add to your `.env` file:

```
WHATSAPP_TEST_PHONE_NUMBER_ID=109876543210
WHATSAPP_TEST_RECIPIENT=15551234567
WHATSAPP_TEST_WABA_ID=102938475610
```

### 6. Run the Tests

```bash
hot test hot/pkg/whatsapp/integration-test/
```

### What the Tests Do

| Test File | What It Tests | Side Effects |
|-----------|--------------|--------------|
| `messages.hot` | Send text message, send location | Sends messages to test recipient |
| `business.hot` | Get phone number info, business profile | Read-only |
| `templates.hot` | List message templates | Read-only |
| `webhooks.hot` | Signature verification, challenge verification | No external calls (unit-style tests) |

### Trial Account Limitations

- Can only send messages to registered test numbers
- Temporary access tokens expire after 24 hours
- Rate limits: 250 messages/second in production, lower in test
- Template messages require Meta approval (the default `hello_world` template is pre-approved)

## Documentation

- [WhatsApp Business Cloud API Documentation](https://developers.facebook.com/docs/whatsapp/cloud-api)
- [Hot Package Documentation](https://hot.dev/pkg/hot.dev/whatsapp)

## License

Apache-2.0 - see [LICENSE](LICENSE)
