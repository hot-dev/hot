# aws-core

AWS core types, authentication (SigV4), and utilities shared by all AWS packages.

## Configuration

All AWS packages require these context variables:

**Required:**
- `aws.access-key-id` - AWS access key ID
- `aws.secret-access-key` - AWS secret access key

**Optional:**
- `aws.session-token` - Session token for temporary credentials (STS, IAM roles)
- `aws.region` - AWS region (defaults to `us-east-1`)

## Provided Functions

- `get-credentials()` - Retrieve AWS credentials from context
- `get-region()` - Get the configured AWS region
- `sign-request()` - Sign requests with AWS Signature V4

## Types

- `Credentials` - AWS credential container
- `AwsError` - Standard error type for AWS API failures

## Documentation

Full documentation available at [hot.dev/pkg/aws-core](https://hot.dev/pkg/aws-core)

## License

Apache-2.0 - see [LICENSE](LICENSE)



