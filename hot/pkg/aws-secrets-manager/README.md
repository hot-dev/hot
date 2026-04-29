# aws-secrets-manager

AWS Secrets Manager API bindings for secure secret storage and retrieval.

## Usage

```hot
::aws::secrets-manager ns

// Get a secret value
secret get-secret-value("my-secret-name")

// Create a new secret
create-secret("my-new-secret", "secret-value")

// Update a secret
put-secret-value("my-secret", "new-value")

// List secrets
secrets list-secrets()
```

## Required IAM Permissions

```json
{
    "Version": "2012-10-17",
    "Statement": [
        {
            "Sid": "SecretsManagerAccess",
            "Effect": "Allow",
            "Action": [
                "secretsmanager:CreateSecret",
                "secretsmanager:GetSecretValue",
                "secretsmanager:PutSecretValue",
                "secretsmanager:UpdateSecret",
                "secretsmanager:DeleteSecret",
                "secretsmanager:RestoreSecret",
                "secretsmanager:DescribeSecret",
                "secretsmanager:RotateSecret"
            ],
            "Resource": "arn:aws:secretsmanager:<REGION>:<ACCOUNT_ID>:secret:<SECRET_PREFIX>*"
        },
        {
            "Sid": "SecretsManagerList",
            "Effect": "Allow",
            "Action": [
                "secretsmanager:ListSecrets"
            ],
            "Resource": "*"
        }
    ]
}
```

Replace `<REGION>`, `<ACCOUNT_ID>`, and `<SECRET_PREFIX>` with your values. The `*` wildcard allows access to all secrets with the given prefix.

## Documentation

Full documentation available at [hot.dev/pkg/aws-secrets-manager](https://hot.dev/pkg/aws-secrets-manager)

## License

Apache-2.0 - see [LICENSE](LICENSE)



