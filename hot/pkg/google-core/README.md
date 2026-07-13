# google-core

Google API foundation for Hot: token acquisition and the authenticated request helper shared by the `google-*` service packages (gmail, drive, sheets, calendar).

## Authentication

Set ONE of these in context (checked in order):

| Context Variables | Flow |
|---|---|
| `google.access.token` | A ready OAuth access token you manage |
| `google.oauth.client-id` + `google.oauth.client-secret` + `google.oauth.refresh-token` | User refresh-token flow (pair with `hot.dev/oauth2` for the initial consent) |
| `google.service.account` (JSON key string), optional `google.impersonate` | Service account via RS256 JWT-bearer (built on `hot.dev/jwt`) |

Service packages request their own scopes through `::google::api/request(scope, method, url, ...)`.

## License

Apache-2.0 - see [LICENSE](LICENSE)
