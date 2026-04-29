# aws-lambda

AWS Lambda API bindings for serverless function invocation and management.

## Usage

```hot
::aws::lambda ns

// Invoke a Lambda function
response invoke("my-function", {key: "value"})

// Invoke asynchronously (fire and forget)
response invoke-async("my-function", {key: "value"})

// List functions
functions list-functions()
```

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



