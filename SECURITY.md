# Security Policy

## Supported Versions

| Version | Supported          |
| ------- | ------------------ |
| 0.1.x   | :white_check_mark: |

## Reporting a Vulnerability

If you discover a security vulnerability in this project, please report it responsibly.

### How to Report

**Do NOT open a public GitHub issue for security vulnerabilities.**

Instead, please report security vulnerabilities via one of these methods:

1. **Email**: Send details to the Cisco Product Security Incident Response Team (PSIRT) at psirt@cisco.com
2. **Cisco Security Vulnerability Policy**: https://sec.cloudapps.cisco.com/security/center/resources/security_vulnerability_policy.html

### What to Include

- Description of the vulnerability
- Steps to reproduce
- Potential impact
- Any suggested fixes (optional)

### Response Timeline

- **Initial Response**: Within 48 hours
- **Status Update**: Within 7 days
- **Resolution Target**: Based on severity

### Severity Classification

| Severity | Response Time | Examples |
|----------|--------------|----------|
| Critical | 24-48 hours | RCE, authentication bypass |
| High | 7 days | Data exposure, privilege escalation |
| Medium | 30 days | Information disclosure |
| Low | 90 days | Minor issues |

## Security Best Practices

When deploying this application:

1. **Run as non-root user** - Use the provided Kubernetes securityContext
2. **Network isolation** - Deploy behind a firewall or VPN
3. **Authentication** - Add an authentication layer if exposing externally
4. **Resource limits** - Configure appropriate CPU/memory limits
5. **Monitoring** - Enable OpenTelemetry tracing for audit trails

## Acknowledgments

We appreciate responsible disclosure and will acknowledge security researchers who report valid vulnerabilities (with their permission).
