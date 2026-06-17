# AWS SNS Package

AWS Simple Notification Service (SNS) bindings for Hot, providing pub/sub messaging and notification capabilities.

## Installation

Add this to the `deps` in your `hot.hot` file:

```hot
"hot.dev/aws-sns": "1.2.1"
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
response ::aws::sns/create-topic("my-topic")

// With attributes
response ::aws::sns/create-topic("my-topic", {
    DisplayName: "My Notifications"
})

// FIFO topic
response ::aws::sns/create-topic("my-topic.fifo", {
    FifoTopic: "true",
    ContentBasedDeduplication: "true"
})
```

### List Topics

```hot
response ::aws::sns/list-topics()
topics response.topics  // Vec<Topic>

// With pagination
response ::aws::sns/list-topics(next-token)
```

### Delete a Topic

```hot
::aws::sns/delete-topic(topic-arn)
```

### Get/Set Topic Attributes

```hot
// Get attributes
attrs ::aws::sns/get-topic-attributes(topic-arn)

// Set a single attribute
::aws::sns/set-topic-attributes(topic-arn, "DisplayName", "New Name")
```

## Subscriptions

### Subscribe to a Topic

```hot
// Email subscription
::aws::sns/subscribe(topic-arn, "email", "user@example.com")

// SQS subscription
::aws::sns/subscribe(topic-arn, "sqs", queue-arn)

// Lambda subscription
::aws::sns/subscribe(topic-arn, "lambda", function-arn)

// HTTP/HTTPS endpoint
::aws::sns/subscribe(topic-arn, "https", "https://example.com/webhook")

// With filter policy
::aws::sns/subscribe(topic-arn, "sqs", queue-arn, {
    FilterPolicy: to-json({ event_type: ["order_placed", "order_shipped"] }),
    RawMessageDelivery: "true"
})
```

### List Subscriptions

```hot
// All subscriptions
subs ::aws::sns/list-subscriptions()

// For a specific topic
subs ::aws::sns/list-subscriptions-by-topic(topic-arn)
```

### Unsubscribe

```hot
::aws::sns/unsubscribe(subscription-arn)
```

### Confirm Subscription (for HTTP/HTTPS)

```hot
::aws::sns/confirm-subscription(topic-arn, confirmation-token)
```

## Publishing Messages

### Simple Publish

```hot
response ::aws::sns/publish(topic-arn, "Hello, World!")
message-id response.message_id
```

### Publish with Subject (for email)

```hot
::aws::sns/publish(topic-arn, "Message body", {
    subject: "Important Notification"
})
```

### Publish with Message Attributes

```hot
::aws::sns/publish-with-attributes(topic-arn, "Order shipped", {
    event_type: { DataType: "String", StringValue: "order_shipped" },
    order_id: { DataType: "String", StringValue: "12345" }
})
```

### Publish Protocol-Specific Messages

```hot
::aws::sns/publish-json(topic-arn, {
    default: "Default message",
    email: "Email-formatted message",
    sqs: to-json({
        type: "notification",
        data: {order_id: "12345"},
    }),
    lambda: to-json({
        action: "process",
        payload: {order_id: "12345"},
    })
})
```

### Batch Publish

```hot
::aws::sns/publish-batch(topic-arn, [
    { id: "1", message: "First message" },
    { id: "2", message: "Second message" },
    {
        id: "3",
        message: "Third message",
        subject: "With subject",
    }
])
```

### FIFO Topic Publishing

```hot
::aws::sns/publish(topic-arn, "Ordered message", {
    message_group_id: "order-123",
    message_deduplication_id: "unique-id-1"
})
```

### Direct SMS

```hot
// Simple SMS
::aws::sns/send-sms("+14155551234", "Your verification code is 123456")

// With options
::aws::sns/send-sms("+14155551234", "Alert!", {
    message_type: "Transactional",
    sender_id: "MyApp"
})
```

## Tagging

```hot
// Add tags
::aws::sns/tag-resource(topic-arn, [
    { Key: "Environment", Value: "production" },
    { Key: "Team", Value: "notifications" }
])

// List tags
tags ::aws::sns/list-tags-for-resource(topic-arn)

// Remove tags
::aws::sns/untag-resource(topic-arn, ["Environment", "Team"])
```

## Error Handling

All functions return a union type of the success response or `AwsError`:

```hot
result ::aws::sns/publish(topic-arn, message)

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
