//! `kito-github`, asked as a Kito server asks it, with a GitHub that is
//! a few lines of this file: what it sends there, and what it makes of
//! the answers.
//!
//! The last test runs it under Kito itself, and needs `kito`, `kitod` and
//! `kito-server` in `KITO_BIN` or in `../kito/target/debug` (`cargo build
//! -p kito-cli -p kitod -p kito-server` there).

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};

/// What the GitHub of the tests does next, and what it was asked.
#[derive(Default)]
struct Fake {
    /// How many times it still says that nobody entered the code.
    pending: u32,
    denied: bool,
    slow: bool,
    /// `METHOD path | authorization | body`, one for each request.
    seen: Vec<String>,
}

struct GitHub {
    host: String,
    fake: Arc<Mutex<Fake>>,
}

const BOB: &str = r#"{"id":583231,"login":"bob","name":"Bob","email":null,"type":"User"}"#;

impl GitHub {
    fn start() -> GitHub {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let host = format!("http://{}", listener.local_addr().unwrap());
        let fake = Arc::new(Mutex::new(Fake::default()));
        let (state, site) = (fake.clone(), host.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                let (mut length, mut authorization) = (0usize, String::from("-"));
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    let line = line.trim().to_owned();
                    if line.is_empty() {
                        break;
                    }
                    let (name, value) = line.split_once(':').unwrap_or((&line, ""));
                    match name.to_ascii_lowercase().as_str() {
                        "content-length" => length = value.trim().parse().unwrap_or(0),
                        "authorization" => authorization = value.trim().to_owned(),
                        _ => {}
                    }
                }
                let mut body = vec![0u8; length];
                reader.read_exact(&mut body).unwrap();
                let body = String::from_utf8_lossy(&body).into_owned();
                let mut parts = first.split(' ');
                let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
                let mut fake = state.lock().unwrap();
                fake.seen
                    .push(format!("{method} {path} | {authorization} | {body}"));
                let has = |field: &str| body.split('&').any(|pair| pair == field);
                let (status, answer) = match (method, path) {
                    ("POST", "/login/device/code") if has("client_id=app-1") => (
                        200,
                        json!({
                            "device_code": "dev-1",
                            "user_code": "WDJB-MJHT",
                            "verification_uri": format!("{site}/login/device"),
                            "expires_in": 900,
                            "interval": 0,
                        })
                        .to_string(),
                    ),
                    ("POST", "/login/oauth/access_token") if has("device_code=dev-1") => {
                        let answer = if fake.denied {
                            json!({ "error": "access_denied" })
                        } else if fake.slow {
                            json!({ "error": "slow_down", "interval": 10 })
                        } else if fake.pending > 0 {
                            fake.pending -= 1;
                            json!({ "error": "authorization_pending" })
                        } else {
                            json!({ "access_token": "gho_once", "token_type": "bearer" })
                        };
                        (200, answer.to_string())
                    }
                    ("POST", "/login/oauth/access_token") => {
                        let answer = match has("code=good")
                            && has("client_secret=shh")
                            && has("client_id=app-1")
                        {
                            true => json!({ "access_token": "gho_once" }),
                            false => json!({ "error": "bad_verification_code" }),
                        };
                        (200, answer.to_string())
                    }
                    ("GET", "/api/v3/user") if authorization == "Bearer gho_once" => {
                        (200, BOB.to_owned())
                    }
                    ("GET", "/api/v3/users/bob") => (200, BOB.to_owned()),
                    ("GET", "/api/v3/users/acme") => (
                        200,
                        r#"{"id":9,"login":"acme","type":"Organization"}"#.to_owned(),
                    ),
                    _ => (404, r#"{"message":"Not Found"}"#.to_owned()),
                };
                drop(fake);
                let head = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: \
                     close\r\n\r\n",
                    answer.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(answer.as_bytes());
            }
        });
        GitHub { host, fake }
    }

    fn seen(&self) -> Vec<String> {
        self.fake.lock().unwrap().seen.clone()
    }
}

/// A server's directory where the plugin is set up for the GitHub of the tests.
fn set_up(dir: &Path, github: &GitHub, secret: bool) {
    let plugin = dir.join("plugins/github");
    std::fs::create_dir_all(&plugin).unwrap();
    let secret = if secret {
        "client_secret = \"shh\"\n"
    } else {
        ""
    };
    std::fs::write(
        plugin.join("config.toml"),
        format!(
            "client_id = \"app-1\"\nhost = \"{}\"\n{secret}",
            github.host
        ),
    )
    .unwrap();
}

/// Runs the plugin as the server does: the server's directory in the
/// environment, what it is told on its input.
fn plugin(dir: &Path, args: &[&str], told: &Value) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_kito-github"))
        .args(args)
        .env("KITO_SERVER_DIR", dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(told.to_string().as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

/// What the plugin answered: the last line it printed.
fn answer(dir: &Path, args: &[&str], told: &Value) -> Value {
    let out = plugin(dir, args, told);
    let (said, errors) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(out.status.success(), "kito-github {args:?}: {said}{errors}");
    serde_json::from_str(said.lines().last().unwrap_or_default())
        .unwrap_or_else(|e| panic!("{e}: {said}"))
}

fn bob() -> Value {
    json!({ "identity": { "provider": "github", "subject": "583231", "login": "bob", "name": "Bob" } })
}

#[test]
fn a_code_entered_at_github_says_who_signs_in_from_a_command_line() {
    let tmp = tempfile::tempdir().unwrap();
    let github = GitHub::start();

    // Not set up: said in words an administrator can act on.
    let out = plugin(
        tmp.path(),
        &["sign-in", "begin"],
        &json!({ "kind": "device" }),
    );
    assert!(!out.status.success());
    let why = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        why.contains("not set up on this server") && why.contains("kito-server github setup"),
        "{why}"
    );

    set_up(tmp.path(), &github, false);
    let begun = answer(
        tmp.path(),
        &["sign-in", "begin"],
        &json!({ "kind": "device" }),
    );
    assert_eq!(
        begun,
        json!({
            "open": format!("{}/login/device", github.host),
            "code": "WDJB-MJHT",
            "state": "dev-1",
            "interval": 0,
        })
    );

    // Nobody entered it yet; then somebody has.
    github.fake.lock().unwrap().pending = 1;
    let finish = json!({ "kind": "device", "state": "dev-1" });
    assert_eq!(
        answer(tmp.path(), &["sign-in", "finish"], &finish),
        json!({ "wait": 5 })
    );
    assert_eq!(answer(tmp.path(), &["sign-in", "finish"], &finish), bob());

    // What GitHub was asked: nothing but who that is, and the token it
    // gave is used once.
    let seen = github.seen();
    assert_eq!(seen[0], "POST /login/device/code | - | client_id=app-1");
    assert!(
        seen[1].ends_with(
            "client_id=app-1&device_code=dev-1&grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code"
        ),
        "{}",
        seen[1]
    );
    assert_eq!(seen[3], "GET /api/v3/user | Bearer gho_once | ");
    assert!(
        !std::fs::read_to_string(tmp.path().join("plugins/github/config.toml"))
            .unwrap()
            .contains("gho_")
    );

    // Asked too often, GitHub says how long to leave it; refused there
    // is refused here.
    github.fake.lock().unwrap().slow = true;
    assert_eq!(
        answer(tmp.path(), &["sign-in", "finish"], &finish),
        json!({ "wait": 10 })
    );
    github.fake.lock().unwrap().denied = true;
    assert_eq!(
        answer(tmp.path(), &["sign-in", "finish"], &finish),
        json!({ "refused": "it was refused at GitHub" })
    );
}

#[test]
fn a_browser_that_comes_back_from_github_says_who_signs_in() {
    let tmp = tempfile::tempdir().unwrap();
    let github = GitHub::start();
    let back = "https://kito.example.com/-/sign-in/github/callback";
    let leaving = json!({ "kind": "browser", "redirect": back });

    // A browser wants the app's secret: without it, said and not tried.
    set_up(tmp.path(), &github, false);
    let none = answer(tmp.path(), &["sign-in", "begin"], &leaving);
    assert!(
        none["refused"].as_str().unwrap().contains("--secret-from"),
        "{none}"
    );

    set_up(tmp.path(), &github, true);
    let begun = answer(tmp.path(), &["sign-in", "begin"], &leaving);
    let state = begun["state"].as_str().unwrap();
    assert!(
        state.len() == 48 && state.bytes().all(|b| b.is_ascii_hexdigit()),
        "{state}"
    );
    assert_eq!(
        begun["open"].as_str().unwrap(),
        format!(
            "{}/login/oauth/authorize?client_id=app-1&redirect_uri=https%3A%2F%2Fkito.example.com%2F-%2Fsign-in%2F\
             github%2Fcallback&state={state}&allow_signup=false",
            github.host
        )
    );
    // Another time, another state.
    assert_ne!(
        answer(tmp.path(), &["sign-in", "begin"], &leaving)["state"],
        begun["state"]
    );

    let came = |query: Value| json!({ "kind": "browser", "state": state, "redirect": back, "query": query });
    assert_eq!(
        answer(
            tmp.path(),
            &["sign-in", "finish"],
            &came(json!({ "code": "good", "state": state }))
        ),
        bob()
    );
    let asked = github
        .seen()
        .into_iter()
        .find(|line| line.starts_with("POST /login/oauth/access_token"))
        .unwrap();
    assert!(
        asked.ends_with(
            "client_id=app-1&client_secret=shh&code=good&redirect_uri=https%3A%2F%2Fkito.example.com%2F-%2Fsign-in\
             %2Fgithub%2Fcallback"
        ),
        "{asked}"
    );

    // A code GitHub does not take, a person who said no there, nobody.
    let stale = answer(
        tmp.path(),
        &["sign-in", "finish"],
        &came(json!({ "code": "old" })),
    );
    assert_eq!(
        stale,
        json!({ "refused": "GitHub no longer takes that code: start again" })
    );
    let no = answer(
        tmp.path(),
        &["sign-in", "finish"],
        &came(json!({ "error": "access_denied" })),
    );
    assert_eq!(no, json!({ "refused": "it was refused at GitHub" }));
    let nobody = answer(tmp.path(), &["sign-in", "finish"], &came(json!({})));
    assert_eq!(
        nobody,
        json!({ "refused": "GitHub sent nobody back: start again" })
    );
}

#[test]
fn a_login_is_looked_up_for_an_administrator() {
    let tmp = tempfile::tempdir().unwrap();
    let github = GitHub::start();
    set_up(tmp.path(), &github, false);
    let lookup = |login: &str| {
        answer(
            tmp.path(),
            &["sign-in", "lookup", login],
            &json!({ "login": login }),
        )
    };
    assert_eq!(lookup("bob"), bob());
    assert_eq!(
        lookup("ghost"),
        json!({ "refused": "GitHub knows nobody called ghost" })
    );
    assert!(lookup("acme")["refused"]
        .as_str()
        .unwrap()
        .contains("is not a person at GitHub"));
    // What cannot be a login is not asked about.
    let before = github.seen().len();
    assert_eq!(
        lookup("../user"),
        json!({ "refused": "`../user` is not a login of GitHub" })
    );
    assert_eq!(github.seen().len(), before);
}

#[test]
fn an_administrator_says_which_oauth_app_is_the_server_s() {
    let tmp = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        let out = plugin(tmp.path(), args, &Value::Null);
        (
            out.status.success(),
            format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ),
        )
    };
    assert!(run(&["github", "status"])
        .1
        .contains("is not set up: `kito-server github setup <client id>`"));
    assert!(!run(&["github", "setup"]).0);

    let (ok, said) = run(&["github", "setup", "Iv1.abc"]);
    assert!(
        ok && said.contains("OAuth app: Iv1.abc") && said.contains("In a browser: no."),
        "{said}"
    );
    let file = tmp.path().join("plugins/github/config.toml");
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "client_id = \"Iv1.abc\"\n"
    );

    // The secret comes from a file, and the file it is kept in is the
    // server's user's only.
    let secret = tmp.path().join("secret");
    std::fs::write(&secret, "s3cret\n").unwrap();
    let (ok, said) = run(&[
        "github",
        "setup",
        "Iv1.abc",
        "--secret-from",
        secret.to_str().unwrap(),
    ]);
    assert!(
        ok && said.contains("In a browser: yes.") && !said.contains("s3cret"),
        "{said}"
    );
    assert!(std::fs::read_to_string(&file)
        .unwrap()
        .contains("client_secret = \"s3cret\""));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    // Set up again without it, the secret stays; a host is https.
    assert!(run(&["github", "setup", "Iv1.new"])
        .1
        .contains("In a browser: yes."));
    assert!(
        !run(&[
            "github",
            "setup",
            "Iv1.new",
            "--host",
            "http://github.example.com"
        ])
        .0
    );
    let (ok, said) = run(&[
        "github",
        "setup",
        "Iv1.new",
        "--host",
        "https://github.example.com",
    ]);
    assert!(
        ok && said.contains("GitHub: https://github.example.com"),
        "{said}"
    );
}

fn kito_bin() -> PathBuf {
    let dir = std::env::var_os("KITO_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../kito/target/debug"));
    let server = dir.join(format!("kito-server{}", std::env::consts::EXE_SUFFIX));
    assert!(
        server.is_file(),
        "{} is not there: this test runs kito-github under Kito. Build it (`cargo build -p kito-cli -p kitod -p \
         kito-server` in the kito repository) or set KITO_BIN to where `kito`, `kitod` and `kito-server` are",
        server.display()
    );
    dir
}

#[test]
fn a_kito_server_signs_people_in_with_it() {
    let bin = kito_bin();
    let exe = |name: &str| bin.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let (dir, config) = (base.join("server"), base.join("config"));
    let github = GitHub::start();
    let server = |args: &[&str]| {
        let mut command = Command::new(exe("kito-server"));
        command
            .arg("--dir")
            .arg(&dir)
            .args(args)
            .env("KITOD_BIN", exe("kitod"))
            .env("KITO_AUTHOR", "the server")
            .env("KITO_CONFIG_DIR", &config);
        command
    };
    let ok = |args: &[&str]| {
        let out = server(args).output().unwrap();
        let (said, errors) = (
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        assert!(out.status.success(), "kito-server {args:?}: {said}{errors}");
        said.into_owned()
    };

    // The plugin as a download leaves it: its program and its manifest.
    let source = base.join("kito-github");
    std::fs::create_dir_all(&source).unwrap();
    let program = format!("kito-github{}", std::env::consts::EXE_SUFFIX);
    std::fs::copy(env!("CARGO_BIN_EXE_kito-github"), source.join(&program)).unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("kito-plugin.toml"),
        source.join("kito-plugin.toml"),
    )
    .unwrap();

    ok(&["init", dir.to_str().unwrap()]);
    ok(&["repo", "add", "shop"]);
    ok(&["plugin", "add", "--path", source.to_str().unwrap()]);
    let enabled = ok(&["plugin", "enable", "github"]);
    assert!(
        enabled.contains("It may: sign_in, server_admin.")
            && enabled.contains("signing in with github"),
        "{enabled}"
    );
    // Its command is the server's, and says what is missing.
    assert!(ok(&["github", "status"]).contains("is not set up"));
    set_up(&dir, &github, true);
    assert!(ok(&["github"]).contains("OAuth app: app-1"));

    // An administrator adds bob by his login; the server keeps his number.
    let added = ok(&[
        "user",
        "add",
        "bob",
        "--identity",
        "github:bob",
        "--role",
        "write",
    ]);
    assert!(
        added.contains("whoever signs in as github:bob is bob. No token."),
        "{added}"
    );
    assert!(std::fs::read_to_string(dir.join("server.toml"))
        .unwrap()
        .contains("identities = [\"github:583231\"]"));
    let ghost = server(&["user", "add", "ghost", "--identity", "github:ghost"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&ghost.stderr)
        .contains("github:ghost: GitHub knows nobody called ghost"));

    struct Serving(std::process::Child);
    impl Drop for Serving {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut serving = Serving(
        server(&["serve", "--plain", "--listen", "127.0.0.1:0"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let mut line = String::new();
    BufReader::new(serving.0.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let address = line
        .trim()
        .strip_prefix("kito-server: listening on ")
        .unwrap()
        .split(',')
        .next()
        .unwrap()
        .to_owned();

    // bob connects by signing in: the code is entered while kito waits.
    github.fake.lock().unwrap().pending = 2;
    let desk = base.join("desk");
    let kito = |args: &[&str]| {
        Command::new(exe("kito"))
            .args(args)
            .current_dir(&base)
            .env("KITOD_BIN", exe("kitod"))
            .env("KITO_NO_CLAUDE", "1")
            .env("KITO_AUTHOR", "bob")
            .env("KITO_CONFIG_DIR", base.join("bob-config"))
            .output()
            .unwrap()
    };
    let connected = kito(&[
        "connect",
        &address,
        "shop",
        desk.to_str().unwrap(),
        "--plain",
        "--sign-in",
        "github",
    ]);
    let (said, asked) = (
        String::from_utf8_lossy(&connected.stdout),
        String::from_utf8_lossy(&connected.stderr),
    );
    assert!(connected.status.success(), "{said}{asked}");
    assert!(
        asked.contains(&format!(
            "To sign in with github, open {}/login/device",
            github.host
        )),
        "{asked}"
    );
    assert!(asked.contains("and enter the code WDJB-MJHT"), "{asked}");
    assert!(
        said.contains("is connected to `shop` on") && said.contains("as bob."),
        "{said}"
    );
    assert_eq!(
        github.fake.lock().unwrap().pending,
        0,
        "kito waited until the code was entered"
    );

    let repos = kito(&["--repo", desk.to_str().unwrap(), "repos"]);
    assert!(
        String::from_utf8_lossy(&repos.stdout).contains("shop"),
        "{}",
        String::from_utf8_lossy(&repos.stderr)
    );
    let _ = kito(&["--repo", desk.to_str().unwrap(), "daemon", "stop"]);
    let _ = kito(&[
        "--repo",
        dir.join("repos/shop").to_str().unwrap(),
        "daemon",
        "stop",
    ]);
}
