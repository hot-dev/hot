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

## Per-call credentials

Pass `opts` as the last argument of any function to use other credentials for that call, so one run can serve many tenants. Nothing is written to context, so this is safe in parallel flows.

A per-call credential is masked in the run's call records, including inside strings like `Bearer ${key}`. A secret shorter than 8 bytes is masked only as a whole value. Records of your own calls that finished before this one may already be written, so mark a credential your code fetches with `::hot::ctx/mark-secret` as soon as you have it.

```hot
::sqs ::aws::sqs

opts {
    access-key-id: "AKIAEXAMPLE",
    secret-access-key: "tenant-secret",
    session-token: "optional-for-temporary-credentials",
    region: "eu-west-1"
}

::sqs/send-message("my-queue-url", "Hello, world!", opts)
::sqs/send-message("my-queue-url", "Hello, world!", 5, {}, opts)
```

`opts` also takes `credentials` (a ready `::aws::core/Credentials` value) and `endpoint` (a base URL such as LocalStack). Without credentials in `opts`, the `aws.*` context variables are used. These are errors, returned before anything is sent:

- `access-key-id`, `secret-access-key` or `credentials` in `opts` but null or blank (leave it out instead); a null or blank `session-token` just means no session token
- half a key pair
- an `endpoint` in `opts` without credentials in `opts`

Where a function's last parameter is already a Map, `opts` comes after it, and a shorter form without that Map isn't offered: pass `{}` or `null` for it.

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



