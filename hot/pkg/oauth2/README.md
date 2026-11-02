# oauth2

OAuth 2.0 client flows for Hot: authorization-code with PKCE, refresh tokens, client credentials, and JWT-bearer (RFC 7523) grants.

## Usage

```hot
::oauth2 ::oauth2

// Authorization-code + PKCE
verifier ::oauth2/pkce-verifier()
url ::oauth2/authorize-url({
  authorize-url: "https://accounts.google.com/o/oauth2/v2/auth",
  client-id: client-id,
  redirect-uri: "https://myapp.hot.dev/oauth/callback",
  scope: "openid email",
  state: state,
  code-challenge: ::oauth2/pkce-challenge(verifier)
})
// ...redirect, then in the callback:
tokens ::oauth2/exchange-code(token-url, {
  client-id: client-id, client-secret: client-secret,
  code: request.query.code, redirect-uri: redirect-uri, code-verifier: verifier
})

// Refresh / machine-to-machine
tokens ::oauth2/refresh(token-url, {client-id: id, client-secret: sec, refresh-token: rt})
tokens ::oauth2/client-credentials(token-url, {client-id: id, client-secret: sec, scope: "api"})

// Service accounts (pair with hot.dev/jwt for the RS256 assertion)
tokens ::oauth2/jwt-bearer("https://oauth2.googleapis.com/token", assertion)
```

Token calls return the provider's decoded JSON body or an Err `{status, body}`.

## Masked credentials

Client secrets, codes, refresh tokens, PKCE verifiers and JWT assertions are masked in the run's call records from when they reach this package, and so are `access_token`, `refresh_token` and `id_token` in the reply, including inside strings and when a provider echoes them in an error. A secret shorter than 8 bytes is masked only as a whole value. Values are returned unchanged.

Records of your own calls that finished before a value reached this package may already be written, so mark a secret your code fetches as soon as you have it:

```hot
::ctx ::hot::ctx

client-secret ::ctx/mark-secret(::hot::env/get("OAUTH_CLIENT_SECRET", ""))
```

Requires Hot 3.0.2.

## License

Apache-2.0 - see [LICENSE](LICENSE)
