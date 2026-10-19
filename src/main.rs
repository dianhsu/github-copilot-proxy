use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Duration, Utc};
use reqwest::Client;
use serde_json::{json, Value};
use std::{
    env,
    error::Error,
    fs,
    io::{self, Write},
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    time::{Duration as StdDuration, Instant},
};
use tokio::sync::Mutex;

const OAUTH_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
const DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const OAUTH_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const TOKEN_URL: &str = "https://api.github.com/copilot_internal/v2/token";
const CHAT_URL: &str = "https://api.githubcopilot.com/chat/completions";
const RESPONSES_URL: &str = "https://api.githubcopilot.com/responses";
const EDITOR_VERSION: &str = "vscode/1.104.1";
const EDITOR_PLUGIN_VERSION: &str = "copilot-chat/0.26.7";
const COPILOT_USER_AGENT: &str = "GitHubCopilotChat/0.26.7";
#[cfg(any(target_os = "macos", test))]
const AUTOSTART_LABEL: &str = "com.dianhsu.github-copilot-proxy";

#[derive(serde::Deserialize, serde::Serialize)]
struct Config {
    #[serde(default = "default_listen_addr")]
    listen_addr: String,
}

fn default_listen_addr() -> String {
    "127.0.0.1:6060".into()
}

#[derive(Clone)]
struct AppState {
    client: Client,
    github_token: String,
    copilot_token: Arc<Mutex<Option<CachedToken>>>,
}

#[derive(Clone)]
struct CachedToken {
    value: String,
    expires_at: DateTime<Utc>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = env::args().skip(1).collect();
    let command = args.first().map(String::as_str);

    if args.is_empty() || (args.len() == 1 && matches!(command, Some("help" | "-h" | "--help"))) {
        print_help();
        return Ok(());
    }

    if args.len() == 1 && matches!(command, Some("-V" | "--version")) {
        println!("github-copilot-proxy {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    if args.len() == 2
        && matches!(args[1].as_str(), "-h" | "--help")
        && matches!(
            command,
            Some("setup" | "verify" | "start" | "status" | "stop")
        )
    {
        print_command_help(command.unwrap());
        return Ok(());
    }

    if args.len() > 1 {
        print_help();
        return Err(io::Error::other(format!("unexpected argument: {}", args[1])).into());
    }

    match command {
        Some("setup") => setup().await?,
        Some("verify") => verify().await?,
        Some("start") => start().await?,
        Some("status") => status().await?,
        Some("stop") => stop().await?,
        Some("__serve") => serve().await?,
        Some(command) => {
            print_help();
            return Err(io::Error::other(format!("unknown command: {command}")).into());
        }
        None => unreachable!(),
    }
    Ok(())
}

fn print_help() {
    println!(
        "\
GitHub Copilot Proxy

Usage: github-copilot-proxy <COMMAND>

Commands:
  setup     Sign in to GitHub Copilot and save local configuration
  verify    Check the saved configuration and Copilot access
  start     Start the proxy as a background service
  status    Show whether the proxy service is running
  stop      Stop the proxy service
  help      Show this help

Options:
  -h, --help     Show this help
  -V, --version  Show version

Run `github-copilot-proxy <COMMAND> --help` for command-specific help."
    );
}

fn print_command_help(command: &str) {
    let (summary, details) = match command {
        "setup" => (
            "Sign in to GitHub Copilot and save local configuration.",
            "Prompts for the listen IP and port first (defaults: 127.0.0.1 and 6060).\n  Uses GITHUB_TOKEN when set; otherwise starts GitHub device authorization.\n  After setup, prompts to install or update the current-user login startup service.",
        ),
        "verify" => (
            "Check the saved configuration and Copilot access.",
            "Requires a completed setup or GITHUB_TOKEN.",
        ),
        "start" => (
            "Start the proxy as a background service.",
            "Requires saved configuration and GitHub credentials. Does not start a duplicate service.",
        ),
        "status" => (
            "Show whether the proxy service is running.",
            "Uses the listen address from the saved configuration.",
        ),
        "stop" => (
            "Stop the proxy service.",
            "Stops the service recorded by the local PID file.",
        ),
        _ => unreachable!(),
    };
    println!(
        "Usage: github-copilot-proxy {command}\n\n{summary}\n\n{details}\n\nOptions:\n  -h, --help  Show this help"
    );
}

async fn setup() -> Result<(), Box<dyn Error>> {
    let config_path = config_path()?;
    let default_addr = setup_default_listen_addr(&config_path)?;
    let listen_addr = prompt_listen_addr(default_addr)?;
    let github_token = match env::var("GITHUB_TOKEN") {
        Ok(token) if !token.is_empty() => token,
        _ => device_login(&Client::new()).await?,
    };

    fetch_copilot_token(&Client::new(), &github_token).await?;
    let config = Config {
        listen_addr: listen_addr.to_string(),
    };
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&config_path, serde_json::to_vec_pretty(&config)?)?;
    let credentials_path = credentials_path()?;
    write_credentials(&credentials_path, &github_token)?;
    println!("Setup complete. Config saved to {}", config_path.display());
    println!("GitHub token saved to {}", credentials_path.display());
    println!("Run `github-copilot-proxy verify` to check access.");
    install_autostart_prompt()?;
    Ok(())
}

fn setup_default_listen_addr(config_path: &Path) -> Result<SocketAddr, Box<dyn Error>> {
    match fs::read_to_string(config_path) {
        Ok(contents) => {
            let config: Config = serde_json::from_str(&contents)?;
            Ok(config.listen_addr.parse()?)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let addr = env::var("LISTEN_ADDR").unwrap_or_else(|_| default_listen_addr());
            Ok(addr.parse()?)
        }
        Err(error) => Err(error.into()),
    }
}

fn prompt_listen_addr(default: SocketAddr) -> Result<SocketAddr, Box<dyn Error>> {
    let ip = loop {
        let value = prompt_value("Listen IP", &default.ip().to_string())?;
        match value.parse::<IpAddr>() {
            Ok(ip) => break ip,
            Err(error) => eprintln!("Invalid IP address: {error}"),
        }
    };

    let port = loop {
        let value = prompt_value("Listen port", &default.port().to_string())?;
        match value.parse::<u16>() {
            Ok(port) if port != 0 => break port,
            Ok(_) => eprintln!("Port must be between 1 and 65535."),
            Err(_) => eprintln!("Enter a port number between 1 and 65535."),
        }
    };

    Ok(SocketAddr::new(ip, port))
}

fn prompt_value(label: &str, default: &str) -> Result<String, Box<dyn Error>> {
    print!("{label} [{default}]: ");
    io::stdout().flush()?;
    let mut value = String::new();
    if io::stdin().read_line(&mut value)? == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "Input ended").into());
    }
    let value = value.trim();
    Ok(if value.is_empty() {
        default.to_owned()
    } else {
        value.to_owned()
    })
}

fn install_autostart_prompt() -> Result<(), Box<dyn Error>> {
    loop {
        print!("Install or update the current-user login startup service? [Y/n] ");
        io::stdout().flush()?;
        let mut answer = String::new();
        if io::stdin().read_line(&mut answer)? == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "Input ended").into());
        }
        match answer.trim().to_ascii_lowercase().as_str() {
            "" | "y" | "yes" => {
                install_autostart(&env::current_exe()?)?;
                println!("Login startup service installed or updated.");
                return Ok(());
            }
            "n" | "no" => {
                println!("Login startup service unchanged.");
                return Ok(());
            }
            _ => println!("Please answer Y or n."),
        }
    }
}

#[cfg(windows)]
fn install_autostart(executable: &Path) -> Result<(), Box<dyn Error>> {
    let task_action = format!("\"{}\" start", executable.display());
    let status = Command::new("schtasks")
        .args([
            "/Create",
            "/SC",
            "ONLOGON",
            "/TN",
            "GitHub Copilot Proxy",
            "/TR",
            &task_action,
            "/F",
            "/RL",
            "LIMITED",
        ])
        .status()?;
    ensure_command_success(status, "Could not install the Windows login task")?;

    let status = Command::new("schtasks")
        .args(["/Run", "/TN", "GitHub Copilot Proxy"])
        .status()?;
    ensure_command_success(status, "Could not start the Windows login task")?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn install_autostart(executable: &Path) -> Result<(), Box<dyn Error>> {
    let plist_path = home_dir()?
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{AUTOSTART_LABEL}.plist"));
    if let Some(parent) = plist_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(
        &plist_path,
        launch_agent_plist(&executable.to_string_lossy()),
    )?;

    let domain = format!("gui/{}", current_uid()?);
    let service = format!("{domain}/{AUTOSTART_LABEL}");
    if Command::new("launchctl")
        .args(["print", &service])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
    {
        let status = Command::new("launchctl")
            .args(["bootout", &service])
            .status()?;
        ensure_command_success(status, "Could not reload the existing macOS login agent")?;
    }
    let status = Command::new("launchctl")
        .args(["bootstrap", &domain, &plist_path.to_string_lossy()])
        .status()?;
    ensure_command_success(status, "Could not install the macOS login agent")?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn install_autostart(executable: &Path) -> Result<(), Box<dyn Error>> {
    let config_home = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or(home_dir()?.join(".config"));
    let unit_path = config_home
        .join("systemd")
        .join("user")
        .join("github-copilot-proxy.service");
    if let Some(parent) = unit_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&unit_path, systemd_user_unit(&executable.to_string_lossy()))?;

    let status = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status()?;
    ensure_command_success(status, "Could not reload systemd user units")?;
    let status = Command::new("systemctl")
        .args(["--user", "enable", "github-copilot-proxy.service"])
        .status()?;
    ensure_command_success(status, "Could not enable the systemd user service")?;
    let status = Command::new("systemctl")
        .args(["--user", "restart", "github-copilot-proxy.service"])
        .status()?;
    ensure_command_success(status, "Could not start the systemd user service")?;
    Ok(())
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
fn install_autostart(_executable: &Path) -> Result<(), Box<dyn Error>> {
    Err(io::Error::other("Login startup services are not supported on this platform").into())
}

#[cfg(any(target_os = "macos", test))]
fn launch_agent_plist(executable: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\"><dict>\n\
         <key>Label</key><string>{}</string>\n\
         <key>ProgramArguments</key><array><string>{}</string><string>start</string></array>\n\
         <key>RunAtLoad</key><true/>\n\
         </dict></plist>\n",
        xml_escape(AUTOSTART_LABEL),
        xml_escape(executable)
    )
}

#[cfg(any(target_os = "macos", test))]
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(target_os = "macos")]
fn current_uid() -> Result<String, Box<dyn Error>> {
    let output = Command::new("id").arg("-u").output()?;
    if !output.status.success() {
        return Err(io::Error::other("Could not determine current user ID").into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

#[cfg(any(target_os = "linux", test))]
fn systemd_user_unit(executable: &str) -> String {
    let executable = systemd_escape(executable);
    format!(
        "[Unit]\nDescription=GitHub Copilot Proxy\n\n\
         [Service]\nType=oneshot\nRemainAfterExit=yes\n\
         ExecStart={executable} start\nExecStop={executable} stop\n\n\
         [Install]\nWantedBy=default.target\n"
    )
}

#[cfg(any(target_os = "linux", test))]
fn systemd_escape(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    format!("\"{escaped}\"")
}

fn home_dir() -> Result<PathBuf, Box<dyn Error>> {
    env::var_os("USERPROFILE")
        .or_else(|| env::var_os("HOME"))
        .map(PathBuf::from)
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "Could not find home directory").into()
        })
}

fn ensure_command_success(
    status: std::process::ExitStatus,
    message: &str,
) -> Result<(), Box<dyn Error>> {
    if !status.success() {
        return Err(io::Error::other(format!("{message} (exit status: {status})")).into());
    }
    Ok(())
}

async fn verify() -> Result<(), Box<dyn Error>> {
    let config = read_config(&config_path()?)?;
    config.listen_addr.parse::<SocketAddr>()?;
    let github_token = github_token()?;
    fetch_copilot_token(&Client::new(), &github_token).await?;
    println!("Configuration and GitHub Copilot access verified.");
    Ok(())
}

async fn start() -> Result<(), Box<dyn Error>> {
    let config = read_config(&config_path()?)?;
    let addr: SocketAddr = config.listen_addr.parse()?;
    let _ = github_token()?;
    let pid_path = pid_path()?;
    if service_is_running(addr).await {
        println!("GitHub Copilot proxy is already running at http://{addr}");
        return Ok(());
    }
    if pid_path.exists() {
        fs::remove_file(&pid_path)?;
    }

    let executable = env::current_exe()?;
    let mut child = Command::new(executable)
        .arg("__serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    for _ in 0..50 {
        if service_is_running(addr).await {
            println!(
                "GitHub Copilot proxy started at http://{addr} (PID {})",
                child.id()
            );
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            return Err(io::Error::other(format!(
                "Proxy process exited during startup with {status}"
            ))
            .into());
        }
        tokio::time::sleep(StdDuration::from_millis(100)).await;
    }
    let _ = terminate_process(child.id());
    let _ = child.wait();
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!("Proxy did not become ready at http://{addr}"),
    )
    .into())
}

async fn serve() -> Result<(), Box<dyn Error>> {
    let config = read_config(&config_path()?)?;
    let addr: SocketAddr = config.listen_addr.parse()?;
    let github_token = github_token()?;
    let state = AppState {
        client: Client::new(),
        github_token,
        copilot_token: Arc::new(Mutex::new(None)),
    };
    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/v1/responses", post(create_response))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|error| io::Error::new(error.kind(), format!("Could not bind {addr}: {error}")))?;
    let pid_path = pid_path()?;
    fs::write(&pid_path, std::process::id().to_string())?;
    let result = axum::serve(listener, app)
        .await
        .map_err(|error| io::Error::other(format!("Proxy server failed: {error}")));
    let _ = fs::remove_file(pid_path);
    result?;
    Ok(())
}

async fn service_is_running(addr: SocketAddr) -> bool {
    let Ok(response) = Client::new()
        .get(format!("http://{addr}/health"))
        .timeout(StdDuration::from_millis(300))
        .send()
        .await
    else {
        return false;
    };
    if !response.status().is_success() {
        return false;
    }
    response.text().await.is_ok_and(|body| body.trim() == "ok")
}

async fn status() -> Result<(), Box<dyn Error>> {
    let config = read_config(&config_path()?)?;
    let addr: SocketAddr = config.listen_addr.parse()?;
    let path = pid_path()?;
    let pid = fs::read_to_string(&path)
        .ok()
        .and_then(|contents| contents.trim().parse::<u32>().ok());

    if service_is_running(addr).await {
        match pid {
            Some(pid) => println!("GitHub Copilot proxy is running at http://{addr} (PID {pid})."),
            None => println!("GitHub Copilot proxy is responding at http://{addr}."),
        }
    } else {
        println!("GitHub Copilot proxy is not running (http://{addr}).");
    }
    Ok(())
}

async fn stop() -> Result<(), Box<dyn Error>> {
    let path = pid_path()?;
    let Ok(contents) = fs::read_to_string(&path) else {
        println!("GitHub Copilot proxy is not running.");
        return Ok(());
    };
    let pid: u32 = match contents.trim().parse() {
        Ok(pid) if pid > 0 => pid,
        _ => {
            fs::remove_file(path)?;
            println!("Removed stale proxy process file; service was not running.");
            return Ok(());
        }
    };
    let config = read_config(&config_path()?)?;
    let addr: SocketAddr = config.listen_addr.parse()?;
    if !service_is_running(addr).await {
        fs::remove_file(path)?;
        println!("GitHub Copilot proxy is not running.");
        return Ok(());
    }

    terminate_process(pid)?;
    for _ in 0..30 {
        if !service_is_running(addr).await {
            let _ = fs::remove_file(&path);
            println!("GitHub Copilot proxy stopped (PID {pid}).");
            return Ok(());
        }
        tokio::time::sleep(StdDuration::from_millis(100)).await;
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!("Proxy process {pid} did not stop"),
    )
    .into())
}

fn terminate_process(pid: u32) -> Result<(), Box<dyn Error>> {
    #[cfg(windows)]
    let result = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .status()?;

    #[cfg(unix)]
    let result = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()?;

    if !result.success() {
        return Err(io::Error::other(format!("Could not stop proxy process {pid}")).into());
    }
    Ok(())
}

fn pid_path() -> Result<PathBuf, Box<dyn Error>> {
    Ok(config_path()?.with_file_name("proxy.pid"))
}

fn config_path() -> Result<PathBuf, Box<dyn Error>> {
    Ok(home_dir()?
        .join(".github-copilot-proxy")
        .join("config.json"))
}

fn read_config(path: &Path) -> Result<Config, Box<dyn Error>> {
    let contents = fs::read_to_string(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "Could not read {}. Run `setup` first: {error}",
                path.display()
            ),
        )
    })?;
    Ok(serde_json::from_str(&contents)?)
}

fn github_token() -> Result<String, Box<dyn Error>> {
    if let Ok(token) = env::var("GITHUB_TOKEN") {
        if !token.is_empty() {
            return Ok(token);
        }
    }
    let path = credentials_path()?;
    let contents = fs::read_to_string(&path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "Could not read {}. Run `setup` first or set GITHUB_TOKEN: {error}",
                path.display()
            ),
        )
    })?;
    let credentials: Credentials = serde_json::from_str(&contents)?;
    Ok(credentials.github_token)
}

fn credentials_path() -> Result<PathBuf, Box<dyn Error>> {
    Ok(config_path()?.with_file_name("credentials.json"))
}

#[derive(serde::Deserialize, serde::Serialize)]
struct Credentials {
    github_token: String,
}

fn write_credentials(path: &Path, token: &str) -> Result<(), Box<dyn Error>> {
    let contents = serde_json::to_vec(&Credentials {
        github_token: token.to_owned(),
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(&contents)?;
        return Ok(());
    }
    #[cfg(not(unix))]
    {
        let mut file = fs::File::create(path)?;
        file.write_all(&contents)?;
        Ok(())
    }
}

#[derive(serde::Deserialize)]
struct DeviceCode {
    device_code: String,
    user_code: String,
    verification_uri: String,
    interval: Option<u64>,
    expires_in: Option<u64>,
}

#[derive(serde::Deserialize)]
struct OAuthToken {
    access_token: Option<String>,
    error: Option<String>,
    interval: Option<u64>,
}

async fn device_login(client: &Client) -> Result<String, Box<dyn Error>> {
    let device: DeviceCode = client
        .post(DEVICE_CODE_URL)
        .header("Accept", "application/json")
        .form(&[("client_id", OAUTH_CLIENT_ID), ("scope", "read:user")])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    println!(
        "Open {} and enter code: {}",
        device.verification_uri, device.user_code
    );
    println!("Waiting for GitHub device authorization...");

    let deadline = Instant::now() + StdDuration::from_secs(device.expires_in.unwrap_or(900));
    let mut interval = device.interval.unwrap_or(5).max(1);
    loop {
        if Instant::now() >= deadline {
            return Err(
                io::Error::new(io::ErrorKind::TimedOut, "GitHub device code expired").into(),
            );
        }
        tokio::time::sleep(StdDuration::from_secs(interval)).await;
        let result: OAuthToken = client
            .post(OAUTH_TOKEN_URL)
            .header("Accept", "application/json")
            .form(&[
                ("client_id", OAUTH_CLIENT_ID),
                ("device_code", device.device_code.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        if let Some(token) = result.access_token.filter(|token| !token.is_empty()) {
            println!("\nGitHub authorization complete.");
            return Ok(token);
        }
        match result.error.as_deref() {
            Some("authorization_pending") => print!("."),
            Some("slow_down") => {
                interval = result.interval.unwrap_or(interval + 5).max(interval + 5);
                print!(".");
            }
            Some("access_denied") => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "GitHub device authorization was denied",
                )
                .into())
            }
            Some("expired_token") => {
                return Err(
                    io::Error::new(io::ErrorKind::TimedOut, "GitHub device code expired").into(),
                )
            }
            Some(error) => {
                return Err(io::Error::other(format!("GitHub OAuth failed: {error}")).into())
            }
            None => return Err(io::Error::other("GitHub OAuth returned no access token").into()),
        }
        io::stdout().flush()?;
    }
}

async fn create_response(
    State(state): State<AppState>,
    Json(request): Json<Value>,
) -> Result<Response, ApiError> {
    let copilot_token = get_copilot_token(&state).await?;
    let chat_request = responses_to_chat(&request)?;

    let upstream = state
        .client
        .post(CHAT_URL)
        .bearer_auth(&copilot_token)
        .header("User-Agent", COPILOT_USER_AGENT)
        .header("Editor-Version", EDITOR_VERSION)
        .header("Editor-Plugin-Version", EDITOR_PLUGIN_VERSION)
        .header("Copilot-Integration-Id", "vscode-chat")
        .json(&chat_request)
        .send()
        .await
        .map_err(|error| ApiError::upstream(format!("Copilot request failed: {error}")))?;

    let status = upstream.status();
    let body = upstream
        .text()
        .await
        .map_err(|error| ApiError::upstream(format!("Could not read Copilot response: {error}")))?;
    if !status.is_success() {
        if is_unsupported_api_for_model(&body) {
            let is_streaming = request.get("stream").and_then(Value::as_bool) == Some(true);
            let fallback = state
                .client
                .post(RESPONSES_URL)
                .bearer_auth(&copilot_token)
                .header("User-Agent", COPILOT_USER_AGENT)
                .header("Editor-Version", EDITOR_VERSION)
                .header("Editor-Plugin-Version", EDITOR_PLUGIN_VERSION)
                .header("Copilot-Integration-Id", "vscode-chat")
                .json(&request)
                .send()
                .await
                .map_err(|error| {
                    ApiError::upstream(format!("Copilot Responses request failed: {error}"))
                })?;
            let fallback_status = fallback.status();
            let fallback_body = fallback.text().await.map_err(|error| {
                ApiError::upstream(format!(
                    "Could not read Copilot Responses response: {error}"
                ))
            })?;
            if !fallback_status.is_success() {
                return Err(ApiError::new(
                    fallback_status,
                    format!("Copilot Responses returned HTTP {fallback_status}: {fallback_body}"),
                ));
            }
            if is_streaming {
                return Ok((
                    [
                        ("content-type", "text/event-stream"),
                        ("cache-control", "no-cache"),
                    ],
                    fallback_body,
                )
                    .into_response());
            }
            let response: Value = serde_json::from_str(&fallback_body).map_err(|error| {
                ApiError::upstream(format!("Invalid Copilot Responses response: {error}"))
            })?;
            return Ok(Json(response).into_response());
        }
        return Err(ApiError::new(
            status,
            format!("Copilot returned HTTP {status}: {body}"),
        ));
    }

    let chat_response: Value = serde_json::from_str(&body)
        .map_err(|error| ApiError::upstream(format!("Invalid Copilot response: {error}")))?;
    let response = chat_to_response(&chat_response);
    if request.get("stream").and_then(Value::as_bool) == Some(true) {
        Ok(sse_response(&response))
    } else {
        Ok(Json(response).into_response())
    }
}

fn is_unsupported_api_for_model(body: &str) -> bool {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|body| {
            body.get("error")
                .and_then(|error| error.get("code"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .as_deref()
        == Some("unsupported_api_for_model")
}

async fn get_copilot_token(state: &AppState) -> Result<String, ApiError> {
    let mut cached = state.copilot_token.lock().await;
    if let Some(token) = cached.as_ref() {
        if token.expires_at > Utc::now() + Duration::minutes(2) {
            return Ok(token.value.clone());
        }
    }

    let token = fetch_copilot_token(&state.client, &state.github_token).await?;
    *cached = Some(token.clone());
    Ok(token.value)
}

async fn fetch_copilot_token(client: &Client, github_token: &str) -> Result<CachedToken, ApiError> {
    let response = client
        .get(TOKEN_URL)
        .header("Authorization", format!("token {github_token}"))
        .header("Accept", "application/json")
        .header("User-Agent", "github-copilot-proxy/0.1")
        .send()
        .await
        .map_err(|error| ApiError::upstream(format!("Copilot token request failed: {error}")))?;
    let status = response.status();
    let body = response.text().await.map_err(|error| {
        ApiError::upstream(format!("Could not read Copilot token response: {error}"))
    })?;
    if !status.is_success() {
        return Err(ApiError::new(
            status,
            format!("Copilot token request returned HTTP {status}: {body}"),
        ));
    }

    let value: Value = serde_json::from_str(&body)
        .map_err(|error| ApiError::upstream(format!("Invalid Copilot token response: {error}")))?;
    let token = value
        .get("token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| ApiError::upstream("Copilot token response did not include a token"))?
        .to_owned();
    let expires_at = value
        .get("expires_at")
        .and_then(Value::as_str)
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
        .or_else(|| {
            value
                .get("expires_in")
                .and_then(Value::as_i64)
                .map(|seconds| Utc::now() + Duration::seconds(seconds))
        })
        .or_else(|| {
            value
                .get("expires_at")
                .and_then(Value::as_i64)
                .and_then(|timestamp| DateTime::from_timestamp(timestamp, 0))
        })
        .ok_or_else(|| {
            ApiError::upstream("Copilot token response did not include a valid expiry")
        })?;

    Ok(CachedToken {
        value: token.clone(),
        expires_at,
    })
}

fn responses_to_chat(request: &Value) -> Result<Value, ApiError> {
    let input = request
        .get("input")
        .ok_or_else(|| ApiError::bad_request("Missing required field: input"))?;
    let mut messages = Vec::new();
    if let Some(instructions) = request.get("instructions").and_then(Value::as_str) {
        messages.push(json!({"role": "system", "content": instructions}));
    }

    match input {
        Value::String(text) => messages.push(json!({"role": "user", "content": text})),
        Value::Array(items) => {
            for item in items {
                let kind = item
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("message");
                if kind == "function_call" {
                    let name = required_str(item, "name")?;
                    let call_id = item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .unwrap_or("call_unknown");
                    let arguments = item
                        .get("arguments")
                        .map(value_to_string)
                        .unwrap_or_else(|| "{}".into());
                    messages.push(json!({
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": call_id,
                            "type": "function",
                            "function": {"name": name, "arguments": arguments}
                        }]
                    }));
                } else if kind == "function_call_output" {
                    let call_id = required_str(item, "call_id")?;
                    let output = item.get("output").map(value_to_string).unwrap_or_default();
                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": call_id,
                        "content": output
                    }));
                } else if kind == "reasoning" {
                    continue;
                } else {
                    let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
                    if !matches!(role, "system" | "developer" | "user" | "assistant" | "tool") {
                        return Err(ApiError::bad_request(format!(
                            "Unsupported input role: {role}"
                        )));
                    }
                    let content = item.get("content").unwrap_or(&Value::Null);
                    let text = content_to_text(content)?;
                    if role == "tool" {
                        messages.push(json!({
                            "role": "tool",
                            "tool_call_id": item.get("tool_call_id").and_then(Value::as_str).unwrap_or(""),
                            "content": text
                        }));
                    } else {
                        let role = if role == "developer" { "system" } else { role };
                        messages.push(json!({"role": role, "content": text}));
                    }
                }
            }
        }
        _ => return Err(ApiError::bad_request("input must be a string or an array")),
    }

    let model = request
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
        .ok_or_else(|| ApiError::bad_request("Missing required field: model"))?;
    let mut chat = json!({
        "model": model,
        "messages": messages,
        "stream": false
    });
    for field in ["temperature", "top_p", "parallel_tool_calls"] {
        if let Some(value) = request.get(field) {
            chat[field] = value.clone();
        }
    }
    if let Some(tool_choice) = request.get("tool_choice") {
        chat["tool_choice"] = if tool_choice.get("type").and_then(Value::as_str) == Some("function")
        {
            json!({
                "type": "function",
                "function": {"name": tool_choice.get("name")}
            })
        } else {
            tool_choice.clone()
        };
    }
    if let Some(max_tokens) = request.get("max_output_tokens") {
        chat["max_tokens"] = max_tokens.clone();
    }
    if let Some(tools) = request.get("tools").and_then(Value::as_array) {
        let mut converted = Vec::new();
        for tool in tools {
            if tool.get("type").and_then(Value::as_str) != Some("function") {
                return Err(ApiError::bad_request(
                    "Only function tools are supported by this proxy",
                ));
            }
            converted.push(json!({
                "type": "function",
                "function": {
                    "name": required_str(tool, "name")?,
                    "description": tool.get("description"),
                    "parameters": tool.get("parameters").cloned().unwrap_or_else(|| json!({"type": "object"})),
                    "strict": tool.get("strict")
                }
            }));
        }
        chat["tools"] = json!(converted);
    }
    Ok(chat)
}

fn content_to_text(content: &Value) -> Result<String, ApiError> {
    if let Some(text) = content.as_str() {
        return Ok(text.to_owned());
    }
    let Some(parts) = content.as_array() else {
        return Ok(String::new());
    };
    let mut text = String::new();
    for part in parts {
        match part.get("type").and_then(Value::as_str) {
            Some("input_text" | "output_text" | "text") => {
                if let Some(value) = part.get("text").and_then(Value::as_str) {
                    text.push_str(value);
                }
            }
            Some("input_image" | "image_url") => {
                return Err(ApiError::bad_request("Image inputs are not supported yet"));
            }
            _ => {}
        }
    }
    Ok(text)
}

fn chat_to_response(chat: &Value) -> Value {
    let choice = chat
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .cloned()
        .unwrap_or(Value::Null);
    let message = choice.get("message").cloned().unwrap_or(Value::Null);
    let chat_id = chat.get("id").and_then(Value::as_str).unwrap_or("unknown");
    let created_at = chat
        .get("created")
        .and_then(Value::as_i64)
        .unwrap_or_else(|| Utc::now().timestamp());
    let model = chat.get("model").cloned().unwrap_or(Value::Null);
    let mut output = Vec::new();

    if let Some(text) = message
        .get("content")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        output.push(json!({
            "id": format!("msg_{chat_id}"),
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text, "annotations": []}]
        }));
    }
    if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
        for tool_call in tool_calls {
            let function = tool_call.get("function").unwrap_or(&Value::Null);
            output.push(json!({
                "id": tool_call.get("id").and_then(Value::as_str).unwrap_or("call_unknown"),
                "type": "function_call",
                "status": "completed",
                "call_id": tool_call.get("id").and_then(Value::as_str).unwrap_or("call_unknown"),
                "name": function.get("name").and_then(Value::as_str).unwrap_or(""),
                "arguments": function.get("arguments").map(value_to_string).unwrap_or_else(|| "{}".into())
            }));
        }
    }

    let usage = chat
        .get("usage")
        .cloned()
        .unwrap_or_else(|| json!({"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0}));
    let status = if choice.get("finish_reason").and_then(Value::as_str) == Some("length") {
        "incomplete"
    } else {
        "completed"
    };
    json!({
        "id": format!("resp_{chat_id}"),
        "object": "response",
        "created_at": created_at,
        "status": status,
        "incomplete_details": if status == "incomplete" {
            json!({"reason": "max_output_tokens"})
        } else {
            Value::Null
        },
        "model": model,
        "output": output,
        "usage": {
            "input_tokens": usage.get("prompt_tokens").and_then(Value::as_i64).unwrap_or(0),
            "output_tokens": usage.get("completion_tokens").and_then(Value::as_i64).unwrap_or(0),
            "total_tokens": usage.get("total_tokens").and_then(Value::as_i64).unwrap_or(0)
        }
    })
}

fn sse_response(response: &Value) -> Response {
    let mut events = String::new();
    let mut in_progress = response.clone();
    in_progress["status"] = json!("in_progress");
    in_progress["output"] = json!([]);
    push_event(
        &mut events,
        "response.created",
        json!({"response": in_progress}),
    );
    push_event(
        &mut events,
        "response.in_progress",
        json!({"response": in_progress}),
    );

    let mut completed_output = Vec::new();
    if let Some(output) = response.get("output").and_then(Value::as_array) {
        for (index, item) in output.iter().enumerate() {
            if item.get("type").and_then(Value::as_str) == Some("message") {
                let mut pending = item.clone();
                pending["status"] = json!("in_progress");
                pending["content"] = json!([]);
                push_event(
                    &mut events,
                    "response.output_item.added",
                    json!({
                        "output_index": index, "item": pending
                    }),
                );
                let text = item["content"][0]["text"].as_str().unwrap_or("");
                let part = json!({"type": "output_text", "text": "", "annotations": []});
                push_event(
                    &mut events,
                    "response.content_part.added",
                    json!({
                        "item_id": item["id"], "output_index": index, "content_index": 0, "part": part
                    }),
                );
                if !text.is_empty() {
                    push_event(
                        &mut events,
                        "response.output_text.delta",
                        json!({
                            "item_id": item["id"], "output_index": index, "content_index": 0, "delta": text
                        }),
                    );
                }
                push_event(
                    &mut events,
                    "response.output_text.done",
                    json!({
                        "item_id": item["id"], "output_index": index, "content_index": 0, "text": text
                    }),
                );
                push_event(
                    &mut events,
                    "response.content_part.done",
                    json!({
                        "item_id": item["id"], "output_index": index, "content_index": 0,
                        "part": item["content"][0]
                    }),
                );
            } else {
                let mut pending = item.clone();
                pending["status"] = json!("in_progress");
                pending["arguments"] = json!("");
                push_event(
                    &mut events,
                    "response.output_item.added",
                    json!({
                        "output_index": index, "item": pending
                    }),
                );
                let arguments = item["arguments"].as_str().unwrap_or("{}");
                if !arguments.is_empty() {
                    push_event(
                        &mut events,
                        "response.function_call_arguments.delta",
                        json!({
                            "item_id": item["id"], "output_index": index, "delta": arguments
                        }),
                    );
                }
                push_event(
                    &mut events,
                    "response.function_call_arguments.done",
                    json!({
                        "item_id": item["id"], "output_index": index, "arguments": arguments
                    }),
                );
            }
            push_event(
                &mut events,
                "response.output_item.done",
                json!({
                    "output_index": index, "item": item
                }),
            );
            completed_output.push(item.clone());
        }
    }
    let mut completed = response.clone();
    completed["output"] = json!(completed_output);
    let terminal_event = if completed["status"] == "incomplete" {
        "response.incomplete"
    } else {
        "response.completed"
    };
    push_event(&mut events, terminal_event, json!({"response": completed}));

    (
        [
            ("content-type", "text/event-stream"),
            ("cache-control", "no-cache"),
        ],
        events,
    )
        .into_response()
}

fn push_event(events: &mut String, event: &str, data: Value) {
    events.push_str(&format!("event: {event}\ndata: {data}\n\n"));
}

fn required_str<'a>(value: &'a Value, field: &str) -> Result<&'a str, ApiError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::bad_request(format!("Missing required field: {field}")))
}

fn value_to_string(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    fn upstream(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, message)
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.message)
    }
}

impl Error for ApiError {}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({"error": {"message": self.message, "type": "proxy_error"}})),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_responses_input_and_tools() {
        let request = json!({
            "model": "gpt-4.1",
            "instructions": "Be concise",
            "input": [
                {"role": "user", "content": [{"type": "input_text", "text": "Hello"}]},
                {"type": "function_call", "call_id": "call_1", "name": "lookup", "arguments": "{\"q\":\"x\"}"}
            ],
            "tools": [{"type": "function", "name": "lookup", "parameters": {"type": "object"}}]
        });
        let chat = responses_to_chat(&request).unwrap();
        assert_eq!(chat["messages"][0]["role"], "system");
        assert_eq!(chat["messages"][1]["content"], "Hello");
        assert_eq!(chat["messages"][2]["tool_calls"][0]["id"], "call_1");
        assert_eq!(chat["tools"][0]["function"]["name"], "lookup");
    }

    #[test]
    fn detects_models_that_require_the_responses_endpoint() {
        assert!(is_unsupported_api_for_model(
            r#"{"error":{"message":"not accessible via /chat/completions","code":"unsupported_api_for_model"}}"#
        ));
        assert!(!is_unsupported_api_for_model(
            r#"{"error":{"code":"some_other_error"}}"#
        ));
        assert!(!is_unsupported_api_for_model("not json"));
    }

    #[test]
    fn creates_login_startup_service_definitions() {
        let plist = launch_agent_plist("C:\\Program Files\\proxy&tool.exe");
        assert!(plist.contains("<string>C:\\Program Files\\proxy&amp;tool.exe</string>"));
        assert!(plist.contains("<string>start</string>"));
        assert!(plist.contains("<key>RunAtLoad</key><true/>"));

        let unit = systemd_user_unit("/opt/Proxy 100%/github-copilot-proxy");
        assert!(unit.contains("Type=oneshot\nRemainAfterExit=yes"));
        assert!(unit.contains("ExecStart=\"/opt/Proxy 100%%/github-copilot-proxy\" start"));
        assert!(unit.contains("ExecStop=\"/opt/Proxy 100%%/github-copilot-proxy\" stop"));
        assert!(unit.contains("WantedBy=default.target"));
    }

    #[test]
    fn defaults_setup_to_loopback_on_port_6060() {
        assert_eq!(default_listen_addr(), "127.0.0.1:6060");
        let addr: SocketAddr = default_listen_addr().parse().unwrap();
        assert_eq!(addr.ip().to_string(), "127.0.0.1");
        assert_eq!(addr.port(), 6060);
    }

    #[test]
    fn converts_chat_tool_call_to_response_item() {
        let response = chat_to_response(&json!({
            "id": "chat_1",
            "model": "gpt-4.1",
            "choices": [{"finish_reason": "tool_calls", "message": {
                "content": null,
                "tool_calls": [{"id": "call_1", "function": {
                    "name": "lookup", "arguments": "{\"q\":\"x\"}"
                }}]
            }}]
        }));
        assert_eq!(response["object"], "response");
        assert_eq!(response["output"][0]["type"], "function_call");
        assert_eq!(response["output"][0]["arguments"], "{\"q\":\"x\"}");
    }
}
