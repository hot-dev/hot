# aws-ses

AWS SES API bindings for sending transactional and marketing emails.

## Usage

```hot
::email ::aws::ses::email

// Send a simple email
::email/send-simple-email(
    "sender@example.com",
    ["recipient@example.com"],
    "Subject",
    "Plain text body",
    "<h1>HTML body</h1>"
)
```

## Per-call credentials

Pass `opts` as the last argument of any function to use other credentials for that call, so one run can serve many tenants. Nothing is written to context, so this is safe in parallel flows.

```hot
::email ::aws::ses::email

opts {
    access-key-id: "AKIAEXAMPLE",
    secret-access-key: "tenant-secret",
    session-token: "optional-for-temporary-credentials",
    region: "eu-west-1"
}

::email/send-simple-email("from@example.com", ["to@example.com"], "Hi", "Hello!", opts)
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
            "Sid": "SESEmailAccess",
            "Effect": "Allow",
            "Action": [
                "ses:SendEmail",
                "ses:SendRawEmail"
            ],
            "Resource": "*",
            "Condition": {
                "StringEquals": {
                    "ses:FromAddress": "<VERIFIED_EMAIL>"
                }
            }
        }
    ]
}
```

Replace `<VERIFIED_EMAIL>` with your verified SES sender email address.

**Note:** The sender email must be verified in SES. If your account is in sandbox mode, recipient emails must also be verified.

## Documentation

Full documentation available at [hot.dev/pkg/aws-ses](https://hot.dev/pkg/aws-ses)

## License

Apache-2.0 - see [LICENSE](LICENSE)



