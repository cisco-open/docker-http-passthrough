# Security

The service is a neutral conduit only when configured to be one. Production deployments should carefully control registry credential sources, network reachability, and logging.

## Credential handling model

```mermaid
flowchart TD
  client["Cisco/client Basic credentials"] --> request["Single HTTP request"]
  request --> service["Passthrough request state"]
  service --> token["Registry token exchange"]
  service --> basic["Direct registry Basic auth"]
  token --> discard["Request completes; credentials not persisted"]
  basic --> discard

  env["Static env credentials"] --> service
  env --> risk["Deployment secret management required"]
```

## Production recommendations

- Use `/registry-auth/...` when the client must choose the registry and identity.
- Leave static registry credentials unset when operating as a neutral conduit.
- Store static credentials in deployment secret managers, not in images or committed files.
- Prefer short-lived or least-privilege registry tokens.
- Remove Cisco `ip http client username/password` after temporary transfers.
- Restrict egress from the passthrough service to approved registries.
- Restrict ingress to trusted Cisco/client networks.
- Enable TLS in front of the service when credentials cross untrusted networks.
- Avoid logging secrets; only non-secret auth type indicators should appear in traces.
- Rotate tokens exposed in terminal transcripts or device configs.

## Threat model overview

```mermaid
flowchart TB
  attacker["Untrusted client"] --> exposed["Public passthrough endpoint"]
  exposed --> ssrf["Arbitrary registry host selection"]
  exposed --> brute["Credential retry attempts"]
  exposed --> disk["Large image disk pressure"]

  controls["Controls"] --> ingress["Limit ingress"]
  controls --> egress["Limit registry egress"]
  controls --> auth["Use /registry-auth for explicit credential challenge"]
  controls --> quotas["Monitor disk and request volume"]
  controls --> telemetry["Trace errors and aborted transfers"]
```

## Sensitive values

Treat these as sensitive:

- `REGISTRY_PASSWORD`
- `REGISTRY_BEARER_TOKEN`
- `REGISTRY_TOKEN`
- Cisco `ip http client password`
- GHCR package-readable tokens
- Docker Hub access tokens
- Enterprise registry robot tokens

Do not encode credentials in filenames or request paths.
