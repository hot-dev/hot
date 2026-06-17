# aws-sqs

AWS SQS API bindings for message queue operations.

## Usage

```hot
::sqs ::aws::sqs

// Send a message
response ::sqs/send-message("my-queue-url", "Hello, world!")

// Receive messages
messages ::sqs/receive-messages("my-queue-url", 10)

// Delete a message after processing
::sqs/delete-message("my-queue-url", receipt-handle)
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



