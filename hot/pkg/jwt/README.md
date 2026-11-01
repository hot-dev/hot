# jwt

JSON Web Tokens (RFC 7519) for Hot. `HS256` (shared secret) and `RS256` (RSA) signing and verification — the two algorithms behind session tokens, OIDC providers, GitHub Apps, and Google service accounts — plus `ES256` (ECDSA P-256) verification for Plaid webhooks and Apple. Claims validation covers `exp`/`nbf`/`iss`/`aud` and an optional `max-age` on `iat`.

## Usage

```hot
::jwt ::jwt

// Sign and verify with a shared secret (HS256)
token ::jwt/sign({sub: "user-42", exp: add(::jwt/now-seconds(), 3600)}, secret)
claims ::jwt/verify(token, secret, {issuer: "https://auth.example.com"})

// RS256 with a PEM private key (Google service accounts, GitHub Apps)
assertion ::jwt/sign({iss: sa.client_email, aud: token-url, ...}, sa.private_key, "RS256")

// RS256 verification straight from JWKS components
claims ::jwt/verify-rs256(token, jwk.n, jwk.e)

// ES256 verification from an EC JWK, rejecting tokens issued over 5 minutes ago
claims ::jwt/verify-es256(token, jwk.x, jwk.y, {max-age: 300})

// Inspect an untrusted token (e.g. pick a JWKS key by kid) — never trust without verify
decoded ::jwt/decode(token)
decoded.header.kid
```

Verification returns the payload claims or an Err describing the failure (bad signature, expired, wrong issuer/audience) — branch with `is-err` / `if-err`. Clock-skew leeway defaults to 60 seconds. `verify-es256` requires Hot 3.0.2 or later; everything else works on 2.6.1.

## License

Apache-2.0 - see [LICENSE](LICENSE)
