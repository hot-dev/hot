# aws-secrets-manager

AWS Secrets Manager API bindings for secure secret storage and retrieval.

## Usage

```hot
::secrets ::aws::secrets-manager

// Get a secret value
secret ::secrets/get-secret-value("my-secret-name")

// Create a new secret
::secrets/create-secret("my-new-secret", "secret-value")

// Update a secret
::secrets/put-secret-value("my-secret", "new-value")

// List secrets
secrets ::secrets/list-secrets()
```

## Per-call credentials

Pass `opts` as the last argument of any function to use other credentials for that call, so one run can serve many tenants. Nothing is written to context, so this is safe in parallel flows.

A per-call credential is masked in the run's call records, including inside strings like `Bearer ${key}`. A secret shorter than 8 bytes is masked only as a whole value. Records of your own calls that finished before this one may already be written, so mark a credential your code fetches with `::hot::ctx/mark-secret` as soon as you have it.

```hot
::secrets ::aws::secrets-manager

opts {
    access-key-id: "AKIAEXAMPLE",
    secret-access-key: "tenant-secret",
    session-token: "optional-for-temporary-credentials",
    region: "eu-west-1"
}

::secrets/get-secret-value("my-app/api-key", opts)
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



