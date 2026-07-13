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

## License

Apache-2.0 - see [LICENSE](LICENSE)
