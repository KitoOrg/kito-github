//! GitHub for Kito: a plugin. On a server, it signs people in (ADR 0006
//! of Kito), which is this file. In a repository, the issues at GitHub are
//! the tasks of lanes: `issues.rs`.
//!
//! It says who somebody is, and nothing else. The server runs it once
//! for each step, with JSON on its input, and reads the last line it
//! prints:
//!
//! - `kito-github sign-in begin`: where the person goes. For a command
//!   line (`{"kind": "device"}`), an address and a code to enter there;
//!   for a browser (`{"kind": "browser", "redirect": …}`), GitHub's page,
//!   which sends the browser back to the server.
//! - `kito-github sign-in finish`: who that was, `{"wait": seconds}`
//!   while they have not done it, or `{"refused": why}`.
//! - `kito-github sign-in lookup <login>`: who a login is, for
//!   `kito-server user add --identity github:<login>`.
//!
//! What it knows of GitHub is the OAuth app of the server, in
//! `plugins/github/config.toml` under the server's directory, which
//! `kito-server github setup` writes. The token GitHub gives it is used
//! once, to ask who it is for, and kept nowhere.

mod issues;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

/// The way to sign in this plugin is: what the server keeps before the `:`.
const METHOD: &str = "github";
const CONFIG_FILE: &str = "config.toml";
/// The server's directory, as Kito says it.
const ENV_SERVER_DIR: &str = "KITO_SERVER_DIR";

/// The OAuth app of this server, and where GitHub is.
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Config {
    client_id: String,
    /// For signing in with a browser. A command line needs none.
    #[serde(skip_serializing_if = "Option::is_none")]
    client_secret: Option<String>,
    /// GitHub itself, or a GitHub Enterprise Server: `https://github.example.com`.
    #[serde(skip_serializing_if = "Option::is_none")]
    host: Option<String>,
    /// Its API, when it is not where it usually is for that host.
    #[serde(skip_serializing_if = "Option::is_none")]
    api: Option<String>,
}

impl Config {
    fn path() -> Result<PathBuf, String> {
        let dir = std::env::var_os(ENV_SERVER_DIR)
            .filter(|dir| !dir.is_empty())
            .ok_or("KITO_SERVER_DIR is not set: this is a plugin of a Kito server, run by `kito-server`")?;
        Ok(PathBuf::from(dir)
            .join("plugins")
            .join(METHOD)
            .join(CONFIG_FILE))
    }

    fn load() -> Result<Config, String> {
        let path = Config::path()?;
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Written for the server's user only: it may hold the app's secret.
    fn save(&self) -> Result<PathBuf, String> {
        use std::io::Write;
        let path = Config::path()?;
        let said = |e: std::io::Error| format!("{}: {e}", path.display());
        std::fs::create_dir_all(path.parent().unwrap_or(&path)).map_err(said)?;
        let text = toml::to_string(self).map_err(|e| e.to_string())?;
        let staged = path.with_extension("toml.new");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        options
            .open(&staged)
            .and_then(|mut file| file.write_all(text.as_bytes()))
            .and_then(|()| std::fs::rename(&staged, &path))
            .map_err(said)?;
        Ok(path)
    }

    fn host(&self) -> String {
        self.host
            .as_deref()
            .unwrap_or("https://github.com")
            .trim_end_matches('/')
            .to_owned()
    }

    fn api(&self) -> String {
        match (&self.api, &self.host) {
            (Some(api), _) => api.trim_end_matches('/').to_owned(),
            // A GitHub Enterprise Server has its API under its own name.
            (None, Some(_)) => format!("{}/api/v3", self.host()),
            (None, None) => "https://api.github.com".to_owned(),
        }
    }

    fn client_id(&self) -> Result<&str, String> {
        match self.client_id.trim() {
            "" => Err("signing in with GitHub is not set up on this server: an administrator runs `kito-server \
                       github setup <client id>` with the OAuth app's"
                .to_owned()),
            id => Ok(id),
        }
    }
}

pub(crate) fn agent() -> ureq::Agent {
    // The server waits a minute for a step: answer well within it.
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout(Duration::from_secs(25))
        .user_agent(concat!("kito-github/", env!("CARGO_PKG_VERSION")))
        .build()
}

/// What GitHub answered, as JSON, whatever the status: its errors are
/// JSON too. `Err` is for a GitHub that could not be asked.
pub(crate) fn answer_of(
    asked: Result<ureq::Response, ureq::Error>,
) -> Result<(u16, Value), String> {
    let response = match asked {
        Ok(response) => response,
        Err(ureq::Error::Status(_, response)) => response,
        Err(e) => return Err(format!("GitHub could not be asked: {e}")),
    };
    let status = response.status();
    let value = response.into_json().unwrap_or(Value::Null);
    Ok((status, value))
}

fn form(url: &str, fields: &[(&str, &str)]) -> Result<(u16, Value), String> {
    answer_of(
        agent()
            .post(url)
            .set("Accept", "application/json")
            .send_form(fields),
    )
}

/// Who somebody is, as the server reads it. The subject is GitHub's
/// number for the account: it stays when the login is renamed.
fn identity_of(user: &Value) -> Result<Value, String> {
    let (Some(id), Some(login)) = (user["id"].as_u64(), user["login"].as_str()) else {
        return Err("GitHub did not say who that is".to_owned());
    };
    let mut identity = json!({ "provider": METHOD, "subject": id.to_string(), "login": login });
    for key in ["name", "email"] {
        if let Some(text) = user[key].as_str().filter(|text| !text.is_empty()) {
            identity[key] = text.into();
        }
    }
    Ok(json!({ "identity": identity }))
}

/// Who a token GitHub just gave is for. The token is not kept.
fn whose(config: &Config, token: &str) -> Result<Value, String> {
    let asked = agent()
        .get(&format!("{}/user", config.api()))
        .set("Accept", "application/vnd.github+json")
        .set("Authorization", &format!("Bearer {token}"))
        .call();
    match answer_of(asked)? {
        (200, user) => identity_of(&user),
        (status, _) => Err(format!(
            "GitHub did not say who signed in (status {status})"
        )),
    }
}

fn refused(why: impl Into<String>) -> Value {
    json!({ "refused": why.into() })
}

/// What GitHub says when it gives no token, in words for the person.
fn no_token(answer: &Value) -> Value {
    let about = answer["error_description"].as_str().unwrap_or_default();
    match answer["error"].as_str().unwrap_or_default() {
        "access_denied" => refused("it was refused at GitHub"),
        "expired_token" => refused("the code ran out before it was entered: start again"),
        "device_flow_disabled" => refused(
            "the OAuth app does not let a code be entered: an administrator enables `Device Flow` in the app's \
             settings at GitHub",
        ),
        "incorrect_client_credentials" => {
            refused("GitHub does not know this server's OAuth app: an administrator checks its client id and secret")
        }
        "bad_verification_code" => refused("GitHub no longer takes that code: start again"),
        "redirect_uri_mismatch" => refused(
            "the OAuth app sends people back to another address than this server's: an administrator sets its \
             callback to <the server's address>/-/sign-in/github/callback",
        ),
        "" => refused("GitHub gave no answer that says who signed in"),
        other => refused(format!("GitHub said `{other}`: {about}")),
    }
}

fn random_state() -> Result<String, String> {
    let mut bytes = [0u8; 24];
    getrandom::getrandom(&mut bytes).map_err(|e| format!("no randomness: {e}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn escaped(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn begin(config: &Config, told: &Value) -> Result<Value, String> {
    let client_id = config.client_id()?;
    if told["kind"] == "browser" {
        let redirect = told["redirect"]
            .as_str()
            .ok_or("the server did not say where to come back to")?;
        if config
            .client_secret
            .as_deref()
            .unwrap_or_default()
            .is_empty()
        {
            return Ok(refused(
                "signing in with a browser is not set up on this server: an administrator runs `kito-server github \
                 setup <client id> --secret-from <file>`",
            ));
        }
        let state = random_state()?;
        // Nothing is asked of GitHub but who the person is: no scope.
        let open = format!(
            "{}/login/oauth/authorize?client_id={}&redirect_uri={}&state={state}&allow_signup=false",
            config.host(),
            escaped(client_id),
            escaped(redirect),
        );
        return Ok(json!({ "open": open, "state": state }));
    }
    let (status, answer) = form(
        &format!("{}/login/device/code", config.host()),
        &[("client_id", client_id)],
    )?;
    let code = (
        answer["device_code"].as_str(),
        answer["user_code"].as_str(),
        answer["verification_uri"].as_str(),
    );
    match code {
        (Some(device), Some(user), Some(open)) => Ok(json!({
            "open": open,
            "code": user,
            // The server keeps it, and hands it back to `finish`.
            "state": device,
            "interval": answer["interval"].as_u64().unwrap_or(5),
        })),
        _ if answer["error"].is_string() => Ok(no_token(&answer)),
        // GitHub answers a client id it does not know with a page.
        _ => Ok(refused(format!(
            "GitHub gave no code to enter (status {status}): an administrator checks the OAuth app's client id, \
             and that `Device Flow` is enabled for it"
        ))),
    }
}

fn finish(config: &Config, told: &Value) -> Result<Value, String> {
    let client_id = config.client_id()?;
    let url = format!("{}/login/oauth/access_token", config.host());
    let answer = if told["kind"] == "browser" {
        let query = &told["query"];
        if let Some(error) = query["error"].as_str() {
            return Ok(no_token(
                &json!({ "error": error, "error_description": query["error_description"] }),
            ));
        }
        let Some(code) = query["code"].as_str().filter(|code| !code.is_empty()) else {
            return Ok(refused("GitHub sent nobody back: start again"));
        };
        let secret = config.client_secret.as_deref().unwrap_or_default();
        let mut fields = vec![
            ("client_id", client_id),
            ("client_secret", secret),
            ("code", code),
        ];
        if let Some(redirect) = told["redirect"].as_str() {
            fields.push(("redirect_uri", redirect));
        }
        form(&url, &fields)?.1
    } else {
        let device = told["state"]
            .as_str()
            .ok_or("the server did not hand back what `begin` gave it")?;
        let fields = [
            ("client_id", client_id),
            ("device_code", device),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ];
        let answer = form(&url, &fields)?.1;
        match answer["error"].as_str() {
            Some("authorization_pending") => return Ok(json!({ "wait": 5 })),
            // Asked too often: GitHub says how long to leave it now.
            Some("slow_down") => {
                return Ok(json!({ "wait": answer["interval"].as_u64().unwrap_or(10) }))
            }
            _ => answer,
        }
    };
    match answer["access_token"].as_str() {
        Some(token) => whose(config, token),
        None => Ok(no_token(&answer)),
    }
}

fn lookup(config: &Config, login: &str) -> Result<Value, String> {
    let fits = !login.is_empty()
        && login.len() <= 39
        && login
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-');
    if !fits {
        return Ok(refused(format!("`{login}` is not a login of GitHub")));
    }
    let asked = agent()
        .get(&format!("{}/users/{login}", config.api()))
        .set("Accept", "application/vnd.github+json")
        .call();
    match answer_of(asked)? {
        (200, user) if user["type"].as_str().is_some_and(|kind| kind != "User") => {
            Ok(refused(format!(
                "{login} is not a person at GitHub: it is of the kind `{}`",
                user["type"].as_str().unwrap_or("?")
            )))
        }
        (200, user) => identity_of(&user),
        (404, _) => Ok(refused(format!("GitHub knows nobody called {login}"))),
        (403 | 429, _) => Err(
            "GitHub answers no more questions from this address for now: try again later"
                .to_owned(),
        ),
        (status, _) => Err(format!(
            "GitHub did not say who {login} is (status {status})"
        )),
    }
}

/// `kito-server github setup | status`: the OAuth app of this server.
fn admin(args: &[String]) -> Result<(), String> {
    let mut config = Config::load()?;
    match args.first().map(String::as_str) {
        Some("setup") => {
            let mut rest = args[1..].iter();
            let (mut id, mut secret, mut host) = (None, None, None);
            while let Some(arg) = rest.next() {
                match arg.as_str() {
                    // From a file: a secret is not written on a command line.
                    "--secret-from" => {
                        let file = rest.next().ok_or("--secret-from wants a file")?;
                        let text =
                            std::fs::read_to_string(file).map_err(|e| format!("{file}: {e}"))?;
                        secret = Some(text.trim().to_owned());
                    }
                    "--host" => host = Some(rest.next().ok_or("--host wants an address")?.clone()),
                    other if other.starts_with('-') => {
                        return Err(format!("unknown option `{other}`"))
                    }
                    other => id = Some(other.to_owned()),
                }
            }
            config.client_id = id.ok_or(
                "say the client id of the OAuth app: `kito-server github setup <client id>`",
            )?;
            if let Some(secret) = secret.filter(|secret| !secret.is_empty()) {
                config.client_secret = Some(secret);
            }
            if let Some(host) = host {
                if !host.starts_with("https://") {
                    return Err("--host is an address that starts with https://".to_owned());
                }
                config.host = Some(host);
            }
            let path = config.save()?;
            println!("Signing in with GitHub is set up ({}).", path.display());
            status(&config);
            Ok(())
        }
        Some("status") | None => {
            status(&config);
            Ok(())
        }
        Some(other) => Err(format!(
            "`{other}`? It is `kito-server github setup <client id> | status`"
        )),
    }
}

fn status(config: &Config) {
    if config.client_id.trim().is_empty() {
        println!("Signing in with GitHub is not set up: `kito-server github setup <client id>`.");
        println!("The client id is the one of an OAuth app at GitHub, with `Device Flow` enabled.");
        return;
    }
    println!("GitHub: {}", config.host());
    println!("OAuth app: {}", config.client_id.trim());
    println!("From a command line (`kito connect … --sign-in github`): yes, if the app has `Device Flow` enabled.");
    match config.client_secret.as_deref().unwrap_or_default().is_empty() {
        true => println!("In a browser: no. It wants the app's secret (`setup <client id> --secret-from <file>`)."),
        false => println!(
            "In a browser: yes. The app's callback is <the server's address>/-/sign-in/github/callback."
        ),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    let done = match words.as_slice() {
        ["sign-in", step, rest @ ..] => {
            let told: Value = serde_json::from_reader(std::io::stdin()).unwrap_or(Value::Null);
            let answer = Config::load().and_then(|config| match (*step, rest) {
                ("begin", _) => begin(&config, &told),
                ("finish", _) => finish(&config, &told),
                ("lookup", [login, ..]) => lookup(&config, login),
                _ => Err("it is `sign-in begin | finish | lookup <login>`".to_owned()),
            });
            answer.map(|answer| println!("{answer}"))
        }
        // As a command of the server, it is given its own name first.
        ["github", ..] => admin(&args[1..]),
        // In a repository: run for what happens there, asked by agents,
        // and `kito github …` for a person (a repository's command is not
        // given its name).
        ["event"] => issues::event(),
        ["tool", name] => {
            println!("{}", issues::tool(name));
            Ok(())
        }
        rest if std::env::var_os("KITO_REPO").is_some() && rest.first() != Some(&"--version") => {
            issues::command(rest)
        }
        ["--version"] => {
            println!("kito-github {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        _ => Err("this is a plugin of Kito. On a server: `kito-server plugin enable github`, to sign people in. \
                  In a repository: `kito plugin enable github`, then `kito github link <owner>/<repository>`"
            .to_owned()),
    };
    match done {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("kito-github: {why}");
            ExitCode::from(1)
        }
    }
}
