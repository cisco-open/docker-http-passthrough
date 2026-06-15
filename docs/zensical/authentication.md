# Registry Authentication

Authentication has two sides:

- Client-to-passthrough: Cisco IOS-XE can supply Basic credentials with `ip http client username` and `ip http client password`.
- Passthrough-to-registry: the service uses anonymous access, Basic auth, static Bearer tokens, or registry-issued Bearer tokens.

## Credential sources

```mermaid
flowchart TD
  start["Registry request needs auth decision"] --> bearer{"REGISTRY_BEARER_TOKEN or REGISTRY_TOKEN set?"}
  bearer -->|yes| static_bearer["Use static Bearer token"]
  bearer -->|no| token_endpoint{"Token endpoint configured or discovered?"}
  token_endpoint -->|yes| token_request["Fetch registry Bearer token"]
  token_endpoint -->|no| basic_available{"Basic credentials available?"}
  basic_available -->|yes| direct_basic["Use direct Basic auth"]
  basic_available -->|no| anonymous["Attempt anonymous pull"]

  token_request --> creds{"Basic credentials available?"}
  creds -->|yes| token_with_basic["Token request with Basic credentials"]
  creds -->|no| token_anonymous["Anonymous token request"]
```

Credential precedence:

1. Static bearer token from `REGISTRY_BEARER_TOKEN` or `REGISTRY_TOKEN`
2. Token endpoint flow using client Basic credentials when supplied
3. Token endpoint flow using static `REGISTRY_USERNAME` and `REGISTRY_PASSWORD`
4. Direct Basic auth using client credentials when supplied
5. Direct Basic auth using static credentials
6. Anonymous pull

## Cisco-supplied credentials

The Cisco HTTP client can be configured globally:

```text
configure terminal
ip http client username <registry-username>
ip http client password <registry-password-or-token>
end
```

The passthrough service challenges with:

```text
HTTP/1.1 401 Unauthorized
WWW-Authenticate: Basic realm="registry credentials"
```

The Cisco device retries the request with a Basic `Authorization` header. The service uses those credentials only for the current upstream registry request or token exchange.

## `/registry-auth` challenge flow

```mermaid
sequenceDiagram
  autonumber
  participant Cisco as Cisco IOS-XE
  participant Service as Passthrough
  participant Registry as Registry
  participant Token as Token service

  Cisco->>Service: GET /registry-auth/https/ghcr.io/docker/org__app_latest.tar
  Service-->>Cisco: 401 Basic challenge
  Cisco->>Service: Retry with Basic credentials
  Service->>Registry: Anonymous manifest request
  Registry-->>Service: 401 Bearer challenge
  Service->>Token: Token request with Cisco Basic credentials
  Token-->>Service: Pull token
  Service->>Registry: Retry manifest/config/layers with Bearer token
  Registry-->>Service: Image content
  Service-->>Cisco: Docker save archive
```

## Auth state machine

```mermaid
stateDiagram-v2
  [*] --> ParseRequest
  ParseRequest --> NeedClientCredentials: route requires auth and no Basic header
  NeedClientCredentials --> ChallengeSent
  ChallengeSent --> ParseRequest: client retries with Basic
  ParseRequest --> SelectRegistry
  SelectRegistry --> TryAnonymous
  TryAnonymous --> BearerChallenge: registry returns Bearer challenge
  TryAnonymous --> BasicChallenge: registry returns Basic challenge
  TryAnonymous --> PullAllowed: registry allows anonymous access
  BearerChallenge --> FetchToken
  FetchToken --> PullAllowed: token issued
  FetchToken --> PullDenied: token rejected
  BasicChallenge --> DirectBasic
  DirectBasic --> PullAllowed: credentials accepted
  DirectBasic --> PullDenied: credentials rejected
  PullAllowed --> StreamArchive
  PullDenied --> [*]
  StreamArchive --> [*]
```

## Neutral conduit mode

For the most neutral model, leave static credential variables unset and use `/registry-auth/...`:

```text
REGISTRY_USERNAME=
REGISTRY_PASSWORD=
REGISTRY_BEARER_TOKEN=
```

The Cisco CLI then supplies both registry identity and selected registry target:

```text
copy http://<passthrough-ip>:8080/registry-auth/https/ghcr.io/docker/my-org__private-image_latest.tar flash:ghcr-image.tar
```

Clean up client credentials when the transfer is finished:

```text
configure terminal
no ip http client username
no ip http client password
end
```
