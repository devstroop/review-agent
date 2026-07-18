# Security Policy

## Threat Model

review-agent is a tool that reads private repository code and sends it to a third-party AI provider for analysis. The following threats are explicitly considered:

### Prompt Injection
A malicious PR diff could attempt to instruct the AI to produce hostile output, leak system prompts, or exfiltrate data.

**Mitigations:**
- AI output is capped at `max_completion_tokens` (default 4096) to limit blast radius
- Markdown blocks matching action directives are stripped before publishing
- The `GITHUB_TOKEN` value is **never** included in the AI prompt context
- Review length is bounded server-side by the model's context window

### Secret Leakage
API keys, tokens, and private diffs must never appear in logs or AI responses.

**Mitigations:**
- Secrets are wrapped in `Sensitive<T>` — Display/Debug impls render as `"***"`
- Raw diffs are logged at DEBUG level only, never at INFO
- HTTP headers containing credentials are never traced
- The binary redacts all secrets before any serialization for output

### Token Scope Abuse
The GitHub token used by review-agent has specific permissions that should not be exceeded.

**Minimum required scopes:**
- `contents: read` — read PR files and metadata
- `pull-requests: write` — post reviews and comments

The default `GITHUB_TOKEN` in GitHub Actions with the following permissions is sufficient:
```yaml
permissions:
  contents: read
  pull-requests: write
```

**Do not use a token with the `repo` scope** — it grants broad access beyond what review-agent needs.

## Reporting a Vulnerability

If you discover a security vulnerability in review-agent, please open a GitHub Issue with the label `security`. Do not disclose the vulnerability publicly until it has been addressed.
