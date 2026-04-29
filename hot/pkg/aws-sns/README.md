# AWS SNS Package

AWS Simple Notification Service (SNS) bindings for Hot, providing pub/sub messaging and notification capabilities.

## Installation

Add this to the `deps` in your `hot.hot` file:

```hot
"hot.dev/aws-sns": "1.1.0"
```

## Features

- **Topics**: Create, delete, and manage SNS topics
- **Subscriptions**: Subscribe endpoints to topics (email, SMS, SQS, Lambda, HTTP/S)
- **Publishing**: Send messages to topics or directly to endpoints
- **Batch Operations**: Publish multiple messages in a single request
- **Message Filtering**: Support for message attributes and filter policies
- **FIFO Topics**: Support for FIFO topics with deduplication and ordering

## Quick Start

```hot
::sns ::aws::sns

// Create a topic
response ::sns/create-topic("my-notifications")
topic-arn response.topic_arn

// Subscribe an email endpoint
::sns/subscribe(topic-arn, "email", "user@example.com")

// Publish a message
::sns/publish(topic-arn, "Hello from Hot!")
```

## Configuration

Required context variables:
- `aws.access-key-id` - AWS access key ID
- `aws.secret-access-key` - AWS secret access key

Optional context variables:
- `aws.session-token` - Session token for temporary credentials
- `aws.region` - AWS region (defaults to `us-east-1`)

## Topics

### Create a Topic

```hot
// Simple topic
response ::sns/create-topic("my-topic")

// With attributes
response ::sns/create-topic("my-topic", {
    DisplayName: "My Notifications"
})

// FIFO topic
response ::sns/create-topic("my-topic.fifo", {
    FifoTopic: "true",
    ContentBasedDeduplication: "true"
})
```

### List Topics

```hot
response ::sns/list-topics()
topics response.topics  // Vec<Topic>

// With pagination
response ::sns/list-topics(next-token)
```

### Delete a Topic

```hot
::sns/delete-topic(topic-arn)
```

### Get/Set Topic Attributes

```hot
// Get attributes
attrs ::sns/get-topic-attributes(topic-arn)

// Set a single attribute
::sns/set-topic-attributes(topic-arn, "DisplayName", "New Name")
```

## Subscriptions

### Subscribe to a Topic

```hot
// Email subscription
::sns/subscribe(topic-arn, "email", "user@example.com")

// SQS subscription
::sns/subscribe(topic-arn, "sqs", queue-arn)

// Lambda subscription
::sns/subscribe(topic-arn, "lambda", function-arn)

// HTTP/HTTPS endpoint
::sns/subscribe(topic-arn, "https", "https://example.com/webhook")

// With filter policy
::sns/subscribe(topic-arn, "sqs", queue-arn, {
    FilterPolicy: to-json({ event_type: ["order_placed", "order_shipped"] }),
    RawMessageDelivery: "true"
})
```

### List Subscriptions

```hot
// All subscriptions
subs ::sns/list-subscriptions()

// For a specific topic
subs ::sns/list-subscriptions-by-topic(topic-arn)
```

### Unsubscribe

```hot
::sns/unsubscribe(subscription-arn)
```

### Confirm Subscription (for HTTP/HTTPS)

```hot
::sns/confirm-subscription(topic-arn, confirmation-token)
```

## Publishing Messages

### Simple Publish

```hot
response ::sns/publish(topic-arn, "Hello, World!")
message-id response.message_id
```

### Publish with Subject (for email)

```hot
::sns/publish(topic-arn, "Message body", {
    subject: "Important Notification"
})
```

### Publish with Message Attributes

```hot
::sns/publish-with-attributes(topic-arn, "Order shipped", {
    event_type: { DataType: "String", StringValue: "order_shipped" },
    order_id: { DataType: "String", StringValue: "12345" }
})
```

### Publish Protocol-Specific Messages

```hot
::sns/publish-json(topic-arn, {
    default: "Default message",
    email: "Email-formatted message",
    sqs: to-json({ type: "notification", data: {...} }),
    lambda: to-json({ action: "process", payload: {...} })
})
```

### Batch Publish

```hot
::sns/publish-batch(topic-arn, [
    { id: "1", message: "First message" },
    { id: "2", message: "Second message" },
    { id: "3", message: "Third message", subject: "With subject" }
])
```

### FIFO Topic Publishing

```hot
::sns/publish(topic-arn, "Ordered message", {
    message_group_id: "order-123",
    message_deduplication_id: "unique-id-1"
})
```

### Direct SMS

```hot
// Simple SMS
::sns/send-sms("+14155551234", "Your verification code is 123456")

// With options
::sns/send-sms("+14155551234", "Alert!", {
    message_type: "Transactional",
    sender_id: "MyApp"
})
```

## Tagging

```hot
// Add tags
::sns/tag-resource(topic-arn, [
    { Key: "Environment", Value: "production" },
    { Key: "Team", Value: "notifications" }
])

// List tags
tags ::sns/list-tags-for-resource(topic-arn)

// Remove tags
::sns/untag-resource(topic-arn, ["Environment", "Team"])
```

## Error Handling

All functions return a union type of the success response or `AwsError`:

```hot
result ::sns/publish(topic-arn, message)

cond {
    is-ok(result) => {
        print(`Published message: ${result.message_id}`)
    }
    => {
        print(`Error: ${result.message}`)
    }
}
```

## Types Reference

### Topic Types

- `Topic { topic_arn: Str }`
- `CreateTopicResponse { topic_arn: Str? }`
- `ListTopicsResponse { topics: Vec<Topic>, next_token: Str? }`
- `GetTopicAttributesResponse { attributes: Map }`

### Subscription Types

- `Subscription { subscription_arn: Str?, owner: Str?, protocol: Str?, endpoint: Str?, topic_arn: Str? }`
- `SubscribeResponse { subscription_arn: Str? }`
- `ListSubscriptionsResponse { subscriptions: Vec<Subscription>, next_token: Str? }`
- `GetSubscriptionAttributesResponse { attributes: Map }`

### Publish Types

- `PublishResponse { message_id: Str?, sequence_number: Str? }`
- `PublishBatchResultEntry { id: Str?, message_id: Str?, sequence_number: Str? }`
- `PublishBatchFailureEntry { id: Str?, code: Str?, message: Str?, sender_fault: Bool? }`
- `PublishBatchResponse { successful: Vec<PublishBatchResultEntry>, failed: Vec<PublishBatchFailureEntry> }`

## License

Apache-2.0
