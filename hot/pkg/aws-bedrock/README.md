# aws-bedrock

AWS Bedrock API bindings for foundation model inference (Claude, Titan, Llama, Mistral, and more).

## Usage

```hot
::bedrock ::aws::bedrock

// Converse API (unified multi-model interface)
// Use cross-region inference profile format: us.<provider>.<model>
response ::bedrock/converse("us.anthropic.claude-3-haiku-20240307-v1:0", [
    {role: "user", content: [{text: "Explain quantum computing"}]}
])

// List available foundation models
models ::bedrock/list-foundation-models()

// Get model details
model ::bedrock/get-foundation-model("anthropic.claude-3-haiku-20240307-v1:0")
```

## Per-call credentials

Pass `opts` as the last argument of any function to use other credentials for that call, so one run can serve many tenants. Nothing is written to context, so this is safe in parallel flows.

A per-call credential is masked in the run's call records, including inside strings like `Bearer ${key}`. A secret shorter than 8 bytes is masked only as a whole value. Records of your own calls that finished before this one may already be written, so mark a credential your code fetches with `::hot::ctx/mark-secret` as soon as you have it.

```hot
::bedrock ::aws::bedrock

opts {
    access-key-id: "AKIAEXAMPLE",
    secret-access-key: "tenant-secret",
    session-token: "optional-for-temporary-credentials",
    region: "eu-west-1"
}

::bedrock/converse("us.amazon.nova-micro-v1:0", "What is 2+2?", opts)
::bedrock/converse-stream("us.amazon.nova-micro-v1:0", [{role: "user", content: [{text: "Hi"}]}], opts)
::aws::bedrock::chat/chat("us.amazon.nova-micro-v1:0", "What is 2+2?", opts)
```

`opts` also takes `credentials` (a ready `::aws::core/Credentials` value) and `endpoint` (a base URL such as LocalStack). Without credentials in `opts`, the `aws.*` context variables are used. These are errors, returned before anything is sent:

- `access-key-id`, `secret-access-key` or `credentials` in `opts` but null or blank (leave it out instead); a null or blank `session-token` just means no session token
- half a key pair
- an `endpoint` in `opts` without credentials in `opts`

Where a function's last parameter is already a Map, `opts` comes after it, and a shorter form without that Map isn't offered: pass `{}` or `null` for it.

## Supported Models

- **Anthropic Claude**: claude-3-opus, claude-3-sonnet, claude-3-haiku, claude-3-5-sonnet
- **Amazon Titan**: titan-text-express, titan-text-lite, titan-embed
- **Meta Llama**: llama-2-13b, llama-2-70b, llama-3-8b, llama-3-70b
- **Mistral AI**: mistral-7b, mixtral-8x7b, mistral-large
- **Cohere**: command, command-light, embed
- **AI21 Labs**: jurassic-2

**Note:** Model invocation requires cross-region inference profiles (e.g., `us.anthropic.claude-3-haiku-20240307-v1:0`).

## Required IAM Permissions

```json
{
    "Version": "2012-10-17",
    "Statement": [
        {
            "Sid": "BedrockAccess",
            "Effect": "Allow",
            "Action": [
                "bedrock:InvokeModel",
                "bedrock:InvokeModelWithResponseStream",
                "bedrock:ListFoundationModels",
                "bedrock:GetFoundationModel"
            ],
            "Resource": "*"
        },
        {
            "Sid": "BedrockMarketplace",
            "Effect": "Allow",
            "Action": [
                "aws-marketplace:ViewSubscriptions",
                "aws-marketplace:Subscribe"
            ],
            "Resource": "*"
        }
    ]
}
```

**Note:** The Marketplace permissions are required for third-party models (Anthropic, Cohere, AI21, Mistral) which are accessed through AWS Marketplace subscriptions.

For more restrictive access, you can limit resources:

```json
{
    "Version": "2012-10-17",
    "Statement": [
        {
            "Sid": "BedrockModelInfo",
            "Effect": "Allow",
            "Action": [
                "bedrock:ListFoundationModels",
                "bedrock:GetFoundationModel"
            ],
            "Resource": "*"
        },
        {
            "Sid": "BedrockInvoke",
            "Effect": "Allow",
            "Action": [
                "bedrock:InvokeModel",
                "bedrock:InvokeModelWithResponseStream"
            ],
            "Resource": [
                "arn:aws:bedrock:*::foundation-model/*",
                "arn:aws:bedrock:*:<ACCOUNT_ID>:inference-profile/*"
            ]
        },
        {
            "Sid": "BedrockMarketplace",
            "Effect": "Allow",
            "Action": [
                "aws-marketplace:ViewSubscriptions",
                "aws-marketplace:Subscribe"
            ],
            "Resource": "*"
        }
    ]
}
```

Replace `<ACCOUNT_ID>` with your AWS account ID.

## Documentation

Full documentation available at [hot.dev/pkg/aws-bedrock](https://hot.dev/pkg/aws-bedrock)

## License

Apache-2.0 - see [LICENSE](LICENSE)



