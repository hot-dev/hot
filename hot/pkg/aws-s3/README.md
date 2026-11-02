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

## Per-call credentials

Pass `opts` as the last argument of any function to use other credentials for that call, so one run can serve many tenants. Nothing is written to context, so this is safe in parallel flows.

A per-call credential is masked in the run's call records, including inside strings like `Bearer ${key}`. A secret shorter than 8 bytes is masked only as a whole value. Records of your own calls that finished before this one may already be written, so mark a credential your code fetches with `::hot::ctx/mark-secret` as soon as you have it.

```hot
::s3 ::aws::s3

opts {
    access-key-id: "AKIAEXAMPLE",
    secret-access-key: "tenant-secret",
    session-token: "optional-for-temporary-credentials",
    region: "eu-west-1"
}

::s3/put-object("my-bucket", "path/to/file.txt", "Hello!", "text/plain", opts)
::s3/get-object("my-bucket", "path/to/file.txt", opts)
::s3/generate-presigned-url("my-bucket", "path/to/file.pdf", 600, opts)
```

`opts` also takes `credentials` (a ready `::aws::core/Credentials` value) and `endpoint` (a base URL such as LocalStack). Without credentials in `opts`, the `aws.*` context variables are used. These are errors, returned before anything is sent:

- `access-key-id`, `secret-access-key` or `credentials` in `opts` but null or blank (leave it out instead); a null or blank `session-token` just means no session token
- half a key pair
- an `endpoint` in `opts` without credentials in `opts`

Where a function's last parameter is already a Map, `opts` comes after it, and a shorter form without that Map isn't offered: pass `{}` or `null` for it.

## Listing with a delimiter

`delimiter` groups keys into one "folder" level. Keys sharing a prefix up to the delimiter come back once in `common_prefixes`. `opts` takes `delimiter` and `start-after`:

```hot
::s3 ::aws::s3

listing ::s3/list-objects("my-bucket", "photos/", {delimiter: "/"})
listing.contents         // objects directly under photos/
listing.common_prefixes  // ["photos/2024/", "photos/2025/"]

// The next page
::s3/list-objects("my-bucket", "photos/", 1000, listing.next_continuation_token, {delimiter: "/"})
```

## Multipart upload

For large objects: start an upload, send parts, then complete it. Every part except the last must be at least 5 MiB. Parts of an unfinished upload are stored and billed until you complete or abort it.

```hot
::s3 ::aws::s3

upload ::s3/create-multipart-upload("my-bucket", "video.mp4", "video/mp4")

part-1 ::s3/upload-part("my-bucket", "video.mp4", upload.upload_id, 1, Bytes("first chunk"))
part-2 ::s3/upload-part("my-bucket", "video.mp4", upload.upload_id, 2, Bytes("last chunk"))

::s3/complete-multipart-upload("my-bucket", "video.mp4", upload.upload_id, [part-1, part-2])

// Or give up and free the parts
::s3/abort-multipart-upload("my-bucket", "video.mp4", upload.upload_id)
```

## S3-compatible stores

Set `endpoint` to use Cloudflare R2, MinIO and other S3-compatible stores. Custom endpoints use path-style URLs (`<endpoint>/<bucket>/<key>`) unless you pass `path-style: false` for `https://<bucket>.<endpoint host>/<key>`. An `endpoint` in `opts` must come with credentials in `opts`: your project's AWS credentials are never sent to a host set per call. To point a whole project at one store, set the `aws.endpoint` context variable instead.

```hot
::s3 ::aws::s3

r2 {
    endpoint: "https://ACCOUNT_ID.r2.cloudflarestorage.com",
    access-key-id: "r2-access-key",
    secret-access-key: "r2-secret",
    region: "auto"
}
::s3/put-object("my-bucket", "file.txt", "Hello!", "text/plain", r2)

minio {
    endpoint: "http://localhost:9000",
    access-key-id: "minioadmin",
    secret-access-key: "minioadmin"
}
::s3/get-object("my-bucket", "file.txt", minio)
```

On AWS itself, requests use the regional virtual-hosted form, `https://<bucket>.s3.<region>.amazonaws.com`. Pass `path-style: true` for `https://s3.<region>.amazonaws.com/<bucket>`. Buckets with dots in their names use path-style automatically, because the TLS certificate doesn't cover them, and so do legacy names with uppercase letters or underscores. A bucket name that isn't valid at all, such as one with `/` or `@`, is an Err before anything is signed.

## Binary objects and file metadata

`get-object` decodes `body` (text as a Str, JSON parsed). Read `body-bytes` for binary objects or the exact stored bytes.

`to-doc-file` normalises an object to a `DocFile`, the same field names as the Google Drive package's: the key is `id` and `path`, and the ETag is `version`. `to-doc-files` does a whole listing, with `common_prefixes` as folders.

```hot
::s3 ::aws::s3

listing ::s3/list-objects("my-bucket", "reports/", {delimiter: "/"})
files ::s3/to-doc-files(listing)
```

## Configuration

Context variables, all optional when you pass credentials in `opts`:
- `aws.access-key-id` - AWS access key ID
- `aws.secret-access-key` - AWS secret access key
- `aws.session-token` - AWS session token (for temporary credentials)
- `aws.region` - AWS region (defaults to us-east-1)
- `aws.endpoint` - base URL of an S3-compatible store

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
                "s3:AbortMultipartUpload",
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



