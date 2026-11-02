# aws-core

AWS core types, authentication (SigV4), and utilities shared by all AWS packages.

## Configuration

AWS packages read these context variables, unless a call passes credentials in `opts`. A call with no credentials from either gets an Err naming them.

- `aws.access-key-id` - AWS access key ID
- `aws.secret-access-key` - AWS secret access key
- `aws.session-token` - Session token for temporary credentials (STS, IAM roles)
- `aws.region` - AWS region (defaults to `us-east-1`)
- `aws.endpoint` - Base URL replacing the AWS endpoints, for LocalStack or an S3-compatible store

## Per-call credentials

Every function in the AWS service packages takes `opts` as its last argument. `resolve(opts)` turns it into the credentials, region and endpoint for that call: `opts` first, then context, then defaults. Nothing is written to context, so one run can serve many tenants, in parallel flows too.

A per-call credential is masked in the run's call records, including inside strings like `Bearer ${key}`. A secret shorter than 8 bytes is masked only as a whole value. Records of your own calls that finished before this one may already be written, so mark a credential your code fetches with `::hot::ctx/mark-secret` as soon as you have it.

```hot
::core ::aws::core

resolved ::core/resolve({
    access-key-id: "AKIAEXAMPLE",
    secret-access-key: "tenant-secret",
    session-token: "optional-for-temporary-credentials",
    region: "eu-west-1"
})
resolved.credentials.access_key_id  // "AKIAEXAMPLE"
resolved.region                     // "eu-west-1"
```

- `access-key-id`, `secret-access-key`, `session-token`: when `opts` has credentials, the context credentials are ignored for the call, session token included. A null or blank `session-token` means no session token.
- `credentials`: a ready `Credentials` value, instead of those keys.
- `region`: signing and endpoint region. It goes into a hostname, so it must be a plain AWS region name like `us-east-1`, from `opts` or context. With a custom endpoint it only signs, so a store's own name like `auto` is fine.
- `endpoint`: base URL replacing the AWS endpoint.

These are errors, so that a tenant with a missing key never runs on the project's credentials:

- `access-key-id`, `secret-access-key` or `credentials` in `opts` but null or blank (leave it out to use context)
- half a key pair, or a session token alone
- an `endpoint` in `opts` without credentials in `opts`: the project's access key id and signatures never go to a host set per call
- a region, bucket or endpoint that could change where the request goes, such as one with `/`, `@`, a backslash or a control character. The error doesn't repeat the value.

## Provided Functions

- `resolve(opts)` - Credentials, region and endpoint for one call
- `get-credentials()` - Retrieve AWS credentials from context
- `get-region()` - Get the configured AWS region
- `sign-request()` - Sign requests with AWS Signature V4; pass an `amz-date` to pin the time
- `sign-url()` - Presign a URL with AWS Signature V4
- `send-signed()` - Sign and send a request with resolved settings
- `uri-encode()`, `uri-encode-path()` - Encode values and paths for a request URL

`sign-request` and `sign-url` take the URL as it will be sent: encode path segments and query values first. S3 signs the path as sent; other services encode it once more, as SigV4 requires.

## Types

- `Credentials` - AWS credential container
- `AwsError` - Standard error type for AWS API failures, with `code`, `message` and `request_id` read from the response

## Documentation

Full documentation available at [hot.dev/pkg/aws-core](https://hot.dev/pkg/aws-core)

## License

Apache-2.0 - see [LICENSE](LICENSE)



