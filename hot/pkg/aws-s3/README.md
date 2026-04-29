# aws-s3

AWS S3 API bindings for object storage, buckets, and presigned URLs.

## Usage

```hot
::s3 ::aws::s3

// Put an object
::s3/put-object("my-bucket", "path/to/file.txt", "Hello, World!", "text/plain")

// Get an object
response ::s3/get-object("my-bucket", "path/to/file.txt")
print(response.body)

// Copy an object
::s3/copy-object("source-bucket", "source-key.txt", "dest-bucket", "dest-key.txt")

// List objects
objects ::s3/list-objects("my-bucket", "prefix/")

// Delete an object
::s3/delete-object("my-bucket", "path/to/file.txt")

// List all buckets
buckets ::s3/list-buckets()
```

## Presigned URLs

Generate presigned URLs for temporary access to S3 objects without credentials:

```hot
::s3 ::aws::s3

// Generate a presigned URL for downloading (GET)
// Default expiration: 1 hour
download-url ::s3/generate-presigned-url("my-bucket", "path/to/file.pdf")

// With custom expiration (in seconds, max 7 days = 604800)
download-url ::s3/generate-presigned-url("my-bucket", "path/to/file.pdf", 86400)

// Generate a presigned URL for uploading (PUT)
upload-url ::s3/generate-presigned-upload-url("my-bucket", "uploads/new-file.pdf")

// Generate a presigned URL for deletion (DELETE)
delete-url ::s3/generate-presigned-delete-url("my-bucket", "path/to/file.pdf")
```

Presigned URLs can be shared with clients for direct browser uploads/downloads without exposing your AWS credentials.

## Configuration

Required context variables:
- `aws.access-key-id` - AWS access key ID
- `aws.secret-access-key` - AWS secret access key

Optional context variables:
- `aws.region` - AWS region (defaults to us-east-1)
- `aws.session-token` - AWS session token (for temporary credentials)

## Required IAM Permissions

```json
{
    "Version": "2012-10-17",
    "Statement": [
        {
            "Sid": "S3BucketAccess",
            "Effect": "Allow",
            "Action": [
                "s3:PutObject",
                "s3:GetObject",
                "s3:DeleteObject",
                "s3:CopyObject",
                "s3:ListBucket",
                "s3:HeadObject",
                "s3:HeadBucket"
            ],
            "Resource": [
                "arn:aws:s3:::<BUCKET_NAME>",
                "arn:aws:s3:::<BUCKET_NAME>/*"
            ]
        },
        {
            "Sid": "S3ListBuckets",
            "Effect": "Allow",
            "Action": [
                "s3:ListAllMyBuckets"
            ],
            "Resource": "*"
        }
    ]
}
```

Replace `<BUCKET_NAME>` with your S3 bucket name.

## Documentation

Full documentation available at [hot.dev/pkg/aws-s3](https://hot.dev/pkg/aws-s3)

## License

Apache-2.0 - see [LICENSE](LICENSE)



