# aws-bedrock

AWS Bedrock API bindings for foundation model inference (Claude, Titan, Llama, Mistral, and more).

## Usage

```hot
::aws::bedrock ns

// Converse API (unified multi-model interface)
// Use cross-region inference profile format: us.<provider>.<model>
response converse("us.anthropic.claude-3-haiku-20240307-v1:0", [
  {role: "user", content: [{text: "Explain quantum computing"}]}
])

// List available foundation models
models list-foundation-models()

// Get model details
model get-foundation-model("anthropic.claude-3-haiku-20240307-v1:0")
```

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



