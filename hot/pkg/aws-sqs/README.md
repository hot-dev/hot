# aws-sqs

AWS SQS API bindings for message queue operations.

## Usage

```hot
::aws::sqs ns

// Send a message
response send-message("my-queue-url", "Hello, world!")

// Receive messages
messages receive-messages("my-queue-url", 10)

// Delete a message after processing
delete-message("my-queue-url", receipt_handle)
```

## Required IAM Permissions

```json
{
    "Version": "2012-10-17",
    "Statement": [
        {
            "Sid": "SQSQueueAccess",
            "Effect": "Allow",
            "Action": [
                "sqs:SendMessage",
                "sqs:ReceiveMessage",
                "sqs:DeleteMessage",
                "sqs:ChangeMessageVisibility",
                "sqs:PurgeQueue",
                "sqs:SendMessageBatch",
                "sqs:DeleteMessageBatch",
                "sqs:GetQueueAttributes",
                "sqs:SetQueueAttributes",
                "sqs:GetQueueUrl"
            ],
            "Resource": "arn:aws:sqs:<REGION>:<ACCOUNT_ID>:<QUEUE_NAME>"
        },
        {
            "Sid": "SQSListQueues",
            "Effect": "Allow",
            "Action": [
                "sqs:ListQueues"
            ],
            "Resource": "*"
        }
    ]
}
```

Replace `<REGION>`, `<ACCOUNT_ID>`, and `<QUEUE_NAME>` with your values.

## Documentation

Full documentation available at [hot.dev/pkg/aws-sqs](https://hot.dev/pkg/aws-sqs)

## License

Apache-2.0 - see [LICENSE](LICENSE)



