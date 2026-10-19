# GitHub Copilot Proxy

A small local adapter that translates OpenAI Responses API requests from Codex into GitHub Copilot requests.

This uses GitHub's undocumented Copilot token, chat, and Responses endpoints. It first tries Chat Completions and automatically retries models that report `unsupported_api_for_model` through the Responses endpoint. These endpoints can change without notice; this is not an official GitHub API or SDK. Use it only with a GitHub account that has Copilot access and in accordance with GitHub's terms.

## Commands

Install Rust to run from source, or install the Windows MSI/macOS PKG from the [GitHub Releases](https://github.com/dianhsu/github-copilot-proxy/releases) page. The installers add `github-copilot-proxy` to the command-line path. Then run:

```powershell
github-copilot-proxy --help
github-copilot-proxy --version
github-copilot-proxy setup
github-copilot-proxy verify
github-copilot-proxy start
github-copilot-proxy status
github-copilot-proxy stop
```

- Running `github-copilot-proxy` without a command displays help. Use `-h` or `--help` for help, including command-specific help such as `github-copilot-proxy setup --help`.
- Use `--version` or `-V` to print the installed version.
- `setup` first prompts for the listen IP and port (first-run defaults: `127.0.0.1` and `6060`; an existing config is offered as the default on later setup runs). It then performs GitHub's OAuth device flow directly: open the URL, enter the one-time code, then wait for authorization. It checks Copilot access and writes local config and credentials files, then asks whether to install the login startup service (`Y` by default). Windows uses Task Scheduler, macOS uses a LaunchAgent, and Linux uses a systemd user unit.
- `verify` checks the saved config and confirms the GitHub token can obtain a Copilot session token.
- `start` starts the proxy as a background service; running it again does not start a duplicate service.
- `status` checks whether the proxy is responding and reports its address and PID when available.
- `stop` stops the running service.

Accepting the setup prompt installs or updates the current user's startup entry and starts the proxy now. If the proxy is already running, the `start` command detects it and avoids launching a duplicate. Declining leaves any existing startup entry unchanged. The startup entry remains installed when using `stop`; it can start the proxy again at the next login. Linux startup installation requires systemd user services.

To run from a source checkout instead, use `cargo run --release -- setup`, `cargo run --release -- verify`, `cargo run --release -- start`, `cargo run --release -- status`, and `cargo run --release -- stop`. The `cargo status` and `cargo stop` aliases work in the repository.

All user data is stored in `%USERPROFILE%\.github-copilot-proxy\` on Windows (or `$HOME/.github-copilot-proxy/` elsewhere): `config.json`, `credentials.json`, and the service PID file. The GitHub OAuth token is stored separately in `credentials.json`; keep your user account secure and do not share this file. On Unix it is created with owner-only permissions. On first setup, `LISTEN_ADDR` can set the initial listen address instead of the default `127.0.0.1:6060`; setup lets you confirm or change its IP and port interactively. `GITHUB_TOKEN` can be set to use an existing token instead of device login.

GitHub Actions uses the `[package].version` in `Cargo.toml` as the release version (`v<version>`). On pushes to `main`, it checks whether that version already has a GitHub Release; if not, it builds Windows x64 and macOS x64/arm64 installers and creates the release with all three packages. Pull requests build the installers for verification, and the workflow can be run manually to publish an unreleased version. Bump the Cargo version to publish the next release. The Windows installer adds its install folder to the system `PATH`; open a new terminal after installation. Packages are currently unsigned; macOS may show an unidentified-developer warning.

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

Choose a model identifier available to your Copilot account. Run `github-copilot-proxy start` before Codex.

This minimal adapter supports text messages and function tools. Image input, other tool types, and genuine token-by-token streaming are not implemented; streaming requests receive a Responses-compatible event sequence after the upstream completion finishes.
