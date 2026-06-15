# Security Policies and Procedures

This document outlines security procedures and general policies for the
`docker-http-passthrough` project.

- [Disclosing a security issue](#disclosing-a-security-issue)
- [Registry credential handling](#registry-credential-handling)
- [Vulnerability management](#vulnerability-management)
- [Suggesting changes](#suggesting-changes)

## Disclosing a security issue

The `docker-http-passthrough` maintainers take all security issues in the project
seriously. Thank you for improving the security of `docker-http-passthrough`. We
appreciate your dedication to responsible disclosure and will make every effort
to acknowledge your contributions.

`docker-http-passthrough` leverages GitHub's private vulnerability reporting.

To learn more about this feature and how to submit a vulnerability report,
review [GitHub's documentation on private reporting](https://docs.github.com/code-security/security-advisories/guidance-on-reporting-and-writing-information-about-vulnerabilities/privately-reporting-a-security-vulnerability).

Here are some helpful details to include in your report:

- a detailed description of the issue
- the steps required to reproduce the issue
- versions of the project that may be affected by the issue
- if known, any mitigations for the issue

A maintainer will acknowledge the report within three (3) business days, and
will send a more detailed response within an additional three (3) business days
indicating the next steps in handling your report.

If you've been unable to successfully draft a vulnerability report via GitHub
or have not received a response during the alloted response window, please
reach out via the [Cisco Open security contact email](mailto:oss-security@cisco.com).

After the initial reply to your report, the maintainers will endeavor to keep
you informed of the progress towards a fix and full announcement, and may ask
for additional information or guidance.

## Registry credential handling

The passthrough service supports three credential patterns:

- static service credentials through `REGISTRY_USERNAME`, `REGISTRY_PASSWORD`, or `REGISTRY_BEARER_TOKEN`
- optional client Basic credentials on `/docker/...` and `/registry/...`
- required client Basic credentials on `/registry-auth/{scheme}/{host}/docker/{filename}`

Registry credentials are never encoded in the image filename. Client-supplied
Basic credentials are used only for the current registry token exchange or
direct Basic registry request, and are not persisted by the passthrough service.

For Cisco IOS-XE clients, remove global HTTP client credentials after a transfer
when they should not remain in running config:

```text
configure terminal
no ip http client username
no ip http client password
end
```

Use least-privilege registry tokens. For GHCR private packages, use a token that
can read only the packages needed for the transfer where possible.

## Vulnerability management

When the maintainers receive a disclosure report, they will assign it to a
primary handler.

This person will coordinate the fix and release process, which involves the
following steps:

- confirming the issue
- determining affected versions of the project
- auditing code to find any potential similar problems
- preparing fixes for all releases under maintenance

## Suggesting changes

If you have suggestions on how this process could be improved please submit an
issue or pull request.
