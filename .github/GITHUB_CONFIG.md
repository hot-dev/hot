# GitHub Configuration

This public repository has two workflow groups:

- `hot.yml`: check/test CI for pushes and pull requests.
- `release.yml`: public release, installer packaging, package CDN publishing, and optional signing.

`GITHUB_TOKEN` is provided by GitHub automatically and does not need to be configured.

## Required For CI

No custom secrets or variables are required for the normal `hot.yml` check/test workflow.

## Required For Public Releases

These are needed only when `release.yml` publishes artifacts to S3/CDN or updates external repositories. Put them in repository settings or organization settings. The public `hot` release workflow intentionally does not bind jobs to GitHub Environments; staging and production environments belong to `hot-cloud`.

| Name | Type | Required When | Description |
|------|------|---------------|-------------|
| `RELEASE_AWS_ROLE_ARN` | Secret | Publishing installers to S3/CDN | AWS OIDC role for `get.hot.dev` installer uploads. |
| `RELEASE_S3_BUCKET` | Variable | Publishing installers to S3/CDN | S3 bucket for installer packages and install scripts. |
| `RELEASE_AWS_REGION` | Variable | Optional | AWS region for installer uploads. Defaults to `us-east-2`. |
| `RELEASE_CLOUDFRONT_DIST_ID` | Variable | Optional | CloudFront distribution to invalidate after installer uploads. |
| `PKG_AWS_ROLE_ARN` | Secret | Publishing package CDN | AWS OIDC role for package CDN uploads. |
| `PKG_S3_BUCKET` | Variable | Publishing package CDN | S3 bucket for package tarballs and package docs. |
| `PKG_AWS_REGION` | Variable | Optional | AWS region for package CDN uploads. Defaults to `us-east-2`. |
| `PKG_CLOUDFRONT_DIST_ID` | Variable | Optional | CloudFront distribution to invalidate for package metadata. |
| `HOMEBREW_TAP_TOKEN` | Secret | Updating Homebrew formula | Token with write access to `hot-dev/homebrew-hot`. |

## Optional Signing Configuration

These are needed only if signed macOS or Windows installers are enabled.

### macOS Signing

| Name | Type | Required When | Description |
|------|------|---------------|-------------|
| `APPLE_SIGNING_ENABLED` | Variable | macOS signing | Set to `true` to import Apple signing certificates. |
| `APPLE_CERTIFICATE_APP_BASE64` | Secret | macOS signing | Base64-encoded Developer ID Application certificate. |
| `APPLE_CERTIFICATE_INST_BASE64` | Secret | macOS signing | Base64-encoded Developer ID Installer certificate. |
| `APPLE_CERTIFICATE_PASSWORD` | Secret | macOS signing | Certificate password. |
| `APPLE_DEVELOPER_ID_APP` | Variable | macOS signing | Developer ID Application identity. |
| `APPLE_DEVELOPER_ID_INST` | Variable | macOS signing | Developer ID Installer identity. |
| `APPLE_ID` | Secret | macOS notarization | Apple ID used for notarization. |
| `APPLE_APP_PASSWORD` | Secret | macOS notarization | App-specific password for notarization. |
| `APPLE_TEAM_ID` | Variable | macOS notarization | Apple team ID. |

### Windows Signing

| Name | Type | Required When | Description |
|------|------|---------------|-------------|
| `TRUSTED_SIGNING_ENABLED` | Variable | Windows signing | Set to `true` to use Azure Trusted Signing. |
| `AZURE_TENANT_ID` | Secret | Windows signing | Azure tenant ID. |
| `AZURE_CLIENT_ID` | Secret | Windows signing | Azure client ID. |
| `AZURE_CLIENT_SECRET` | Secret | Windows signing | Azure client secret. |
| `TRUSTED_SIGNING_ENDPOINT` | Variable | Windows signing | Azure Trusted Signing endpoint. |
| `TRUSTED_SIGNING_ACCOUNT_NAME` | Variable | Windows signing | Trusted Signing account name. |
| `TRUSTED_SIGNING_CERTIFICATE_PROFILE` | Variable | Windows signing | Certificate profile name. |

## Release Inputs

Manual `release.yml` runs expose:

| Input | Description |
|-------|-------------|
| `publish_to_cdn` | When `true`, publish installer and package CDN artifacts if the relevant S3 variables are configured. |

Stable releases should use immutable `vX.Y.Z` tags or the `stable` branch with a new `resources/version.txt` patch version.
