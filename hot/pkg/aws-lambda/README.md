# aws-lambda

AWS Lambda API bindings for serverless function invocation and management.

## Usage

```hot
::lambda ::aws::lambda

// Invoke a Lambda function
response ::lambda/invoke("my-function", {key: "value"})

// Invoke asynchronously (fire and forget)
response ::lambda/invoke-async("my-function", {key: "value"})

// List functions
functions ::lambda/list-functions()
```

## Per-call credentials

Pass `opts` as the last argument of any function to use other credentials for that call, so one run can serve many tenants. Nothing is written to context, so this is safe in parallel flows.

A per-call credential is masked in the run's call records, including inside strings like `Bearer ${key}`. A secret shorter than 8 bytes is masked only as a whole value. Records of your own calls that finished before this one may already be written, so mark a credential your code fetches with `::hot::ctx/mark-secret` as soon as you have it.

```hot
::lambda ::aws::lambda

opts {
    access-key-id: "AKIAEXAMPLE",
    secret-access-key: "tenant-secret",
    session-token: "optional-for-temporary-credentials",
    region: "eu-west-1"
}

::lambda/invoke("my-function", {name: "Hot"}, opts)
::lambda/invoke("my-function", null, opts)
```

`opts` also takes `credentials` (a ready `::aws::core/Credentials` value) and `endpoint` (a base URL such as LocalStack). Without credentials in `opts`, the `aws.*` context variables are used. These are errors, returned before anything is sent:

- `access-key-id`, `secret-access-key` or `credentials` in `opts` but null or blank (leave it out instead); a null or blank `session-token` just means no session token
- half a key pair
- an `endpoint` in `opts` without credentials in `opts`

Where a function's last parameter is already a Map, `opts` comes after it, and a shorter form without that Map isn't offered: pass `{}` or `null` for it.

`invoke(name, payload)` takes a Map as the payload, so `opts` always comes after the payload.
## Required IAM Permissions

```json
{
    "Version": "2012-10-17",
    "Statement": [
        {
            "Sid": "LambdaInvokeAccess",
            "Effect": "Allow",
            "Action": [
                "lambda:InvokeFunction",
                "lambda:GetFunction",
                "lambda:GetFunctionConfiguration"
            ],
            "Resource": "arn:aws:lambda:<REGION>:<ACCOUNT_ID>:function:<FUNCTION_NAME>"
        },
        {
            "Sid": "LambdaListFunctions",
            "Effect": "Allow",
            "Action": [
                "lambda:ListFunctions"
            ],
            "Resource": "*"
        }
    ]
}
```

Replace `<REGION>`, `<ACCOUNT_ID>`, and `<FUNCTION_NAME>` with your values. Use `*` for `<FUNCTION_NAME>` to allow access to all functions.

## Documentation

Full documentation available at [hot.dev/pkg/aws-lambda](https://hot.dev/pkg/aws-lambda)

## License

Apache-2.0 - see [LICENSE](LICENSE)



