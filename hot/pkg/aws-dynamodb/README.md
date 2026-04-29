# aws-dynamodb

AWS DynamoDB API bindings for NoSQL database operations.

## Usage

```hot
::aws::dynamodb ns

// Put an item
put-item("my-table", {id: {S: "123"}, name: {S: "Alice"}})

// Get an item
item get-item("my-table", {id: {S: "123"}})

// Query items
results query("my-table", "id = :id", {":id": {S: "123"}})

// Scan table
all-items scan("my-table")
```

## Required IAM Permissions

```json
{
    "Version": "2012-10-17",
    "Statement": [
        {
            "Sid": "DynamoDBTableAccess",
            "Effect": "Allow",
            "Action": [
                "dynamodb:PutItem",
                "dynamodb:GetItem",
                "dynamodb:DeleteItem",
                "dynamodb:UpdateItem",
                "dynamodb:Query",
                "dynamodb:Scan",
                "dynamodb:BatchGetItem",
                "dynamodb:BatchWriteItem",
                "dynamodb:DescribeTable"
            ],
            "Resource": [
                "arn:aws:dynamodb:<REGION>:<ACCOUNT_ID>:table/<TABLE_NAME>",
                "arn:aws:dynamodb:<REGION>:<ACCOUNT_ID>:table/<TABLE_NAME>/index/*"
            ]
        },
        {
            "Sid": "DynamoDBListTables",
            "Effect": "Allow",
            "Action": [
                "dynamodb:ListTables"
            ],
            "Resource": "*"
        }
    ]
}
```

Replace `<REGION>`, `<ACCOUNT_ID>`, and `<TABLE_NAME>` with your values.

## Documentation

Full documentation available at [hot.dev/pkg/aws-dynamodb](https://hot.dev/pkg/aws-dynamodb)

## License

Apache-2.0 - see [LICENSE](LICENSE)



