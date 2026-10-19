# GitHub Copilot Proxy

A small local adapter that translates OpenAI Responses API requests from Codex into GitHub Copilot Chat Completions requests.

This uses GitHub's undocumented Copilot token and chat endpoints. They can change without notice; this is not an official GitHub API or SDK. Use it only with a GitHub account that has Copilot access and in accordance with GitHub's terms.

## Commands

Install Rust, then run one of the three commands:

```powershell
cargo run --release -- setup
cargo run --release -- verify
cargo run --release
cargo status
cargo stop
```

- `setup` performs GitHub's OAuth device flow directly: open the URL, enter the one-time code, then wait for authorization. It checks Copilot access and writes local config and credentials files.
- `verify` checks the saved config and confirms the GitHub token can obtain a Copilot session token.
- `cargo run` (or `run`) starts the proxy as a background service using the saved config; running it again does not start a duplicate service.
- `cargo status` checks whether the proxy is responding and reports its address and PID when available.
- `cargo stop` stops the running service. You can also use `cargo run --release -- stop`.

All files are stored in `%USERPROFILE%\.github-copilot-proxy\` on Windows (or `$HOME/.github-copilot-proxy/` elsewhere): `config.json`, `credentials.json`, and the service PID file. The GitHub OAuth token is stored separately in `credentials.json`; keep your user account secure and do not share this file. On Unix it is created with owner-only permissions. `setup` defaults to `127.0.0.1:8080`; set `LISTEN_ADDR` before setup to choose another address. `GITHUB_TOKEN` can be set to use an existing token instead of device login.

The device-login flow follows the approach used by [OpenClaw](https://github.com/openclaw/openclaw/blob/main/docs/providers/github-copilot.md) and [Hermes Agent](https://github.com/NousResearch/hermes-agent/blob/main/hermes_cli/copilot_auth.py). It uses an OAuth client ID and Copilot token endpoint that are not a documented public Copilot API; GitHub may change or restrict them.

The server exposes `GET /health` and `POST /v1/responses`. Keep it on loopback; the proxy does not authenticate incoming requests.

## Codex

Add a provider to `~/.codex/config.toml`:

```toml
model_provider = "copilot-proxy"
model = "gpt-4.1"

[model_providers.copilot-proxy]
name = "GitHub Copilot Proxy"
base_url = "http://127.0.0.1:8080/v1"
wire_api = "responses"
requires_openai_auth = false
```

Choose a model identifier available to your Copilot account. Run `github-copilot-proxy run` before Codex.

This minimal adapter supports text messages and function tools. Image input, other tool types, and genuine token-by-token streaming are not implemented; streaming requests receive a Responses-compatible event sequence after the upstream completion finishes.
