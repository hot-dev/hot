# aws-ses

AWS SES API bindings for sending transactional and marketing emails.

## Usage

```hot
::aws::ses::email ns

// Send a simple email
send-simple-email(
    "sender@example.com",
    ["recipient@example.com"],
    "Subject",
    "Plain text body",
    "<h1>HTML body</h1>"
)
```

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



