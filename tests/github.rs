//! `kito-github`, asked as a Kito server asks it, with a GitHub that is
//! a few lines of this file: what it sends there, and what it makes of
//! the answers.
//!
//! The last two tests run it under Kito itself, and need `kito`, `kitod` and
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
    /// The key of the GitHub App it makes.
    pem: String,
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
                        let answer = match has("code=good") && has("client_secret=shh") && has("client_id=app-1") {
                            true => json!({ "access_token": "gho_once" }),
                            false => json!({ "error": "bad_verification_code" }),
                        };
                        (200, answer.to_string())
                    }
                    ("GET", "/api/v3/user") if authorization == "Bearer gho_once" => (200, BOB.to_owned()),
                    ("GET", "/api/v3/users/bob") => (200, BOB.to_owned()),
                    ("GET", "/api/v3/users/acme") => {
                        (200, r#"{"id":9,"login":"acme","type":"Organization"}"#.to_owned())
                    }
                    // Issues, for a repository whose lanes are for them. A pull request is listed among them.
                    ("GET", "/api/v3/repos/acme/shop/issues?state=open&per_page=30") => (
                        200,
                        json!([
                            { "number": 12, "title": "Prices lose half a cent", "labels": [{ "name": "bug" }] },
                            { "number": 13, "title": "A pull request", "labels": [], "pull_request": {} },
                        ])
                        .to_string(),
                    ),
                    ("GET", "/api/v3/repos/acme/shop/issues/12") => (
                        200,
                        json!({
                            "number": 12, "title": "Prices lose half a cent", "state": "open", "comments": 1,
                            "user": { "login": "carol" }, "labels": [{ "name": "bug" }],
                            "html_url": "https://github.com/acme/shop/issues/12",
                            "body": "Every third order is half a cent short.",
                        })
                        .to_string(),
                    ),
                    ("GET", "/api/v3/repos/acme/shop/issues/12/comments?per_page=5") => {
                        (200, json!([{ "user": { "login": "ada" }, "body": "Keep amounts in cents." }]).to_string())
                    }
                    // The GitHub App of a server (kito-github 0.4).
                    ("POST", "/api/v3/app-manifests/abc/conversions") => (
                        201,
                        json!({
                            "id": 7, "slug": "kito-test", "owner": { "login": "acme" },
                            "html_url": format!("{site}/apps/kito-test"), "webhook_secret": "whsec",
                            "pem": fake.pem, "client_id": "app-1", "client_secret": "shh",
                        })
                        .to_string(),
                    ),
                    ("GET", "/api/v3/repos/acme/shop/installation") if authorization.starts_with("Bearer ey") => {
                        (200, r#"{"id":77}"#.to_owned())
                    }
                    ("POST", "/api/v3/app/installations/77/access_tokens") if authorization.starts_with("Bearer ey") => {
                        (201, r#"{"token":"ghs_inst"}"#.to_owned())
                    }
                    ("GET", "/api/v3/app/installations") if authorization.starts_with("Bearer ey") => {
                        (200, r#"[{"id":77}]"#.to_owned())
                    }
                    ("GET", "/api/v3/installation/repositories?per_page=100&page=1") if authorization == "Bearer ghs_inst" => (
                        200,
                        json!({ "repositories": [{ "full_name": "acme/shop", "private": true, "default_branch": "main" }] })
                            .to_string(),
                    ),
                    ("POST", "/api/v3/repos/acme/shop/issues/12/comments") | ("PATCH", "/api/v3/repos/acme/shop/issues/12")
                        if authorization == "Bearer ghp_repo" || authorization == "Bearer ghs_inst" =>
                    {
                        (201, "{}".to_owned())
                    }
                    ("POST", "/api/v3/repos/acme/shop/issues/12/comments") => (401, r#"{"message":"Bad credentials"}"#.to_owned()),
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
        enabled.contains("It may: read, write, integrate, admin, sign_in, pages, server_admin.")
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
        "--repo",
        "shop:write",
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

/// Waits until the GitHub of the tests was asked something that starts
/// with `start` at least `times` times: events are told after the fact.
fn asked(github: &GitHub, start: &str, times: usize) -> Vec<String> {
    let begun = std::time::Instant::now();
    loop {
        let found: Vec<String> = github
            .seen()
            .into_iter()
            .filter(|line| line.starts_with(start))
            .collect();
        if found.len() >= times {
            return found;
        }
        assert!(
            begun.elapsed() < std::time::Duration::from_secs(20),
            "GitHub was not asked `{start}` {times} times: {:?}",
            github.seen()
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[test]
fn a_lane_whose_task_is_an_issue_tells_the_issue() {
    let bin = kito_bin();
    let exe = |name: &str| bin.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let (root, config) = (base.join("shop"), base.join("config"));
    std::fs::create_dir_all(&root).unwrap();
    let github = GitHub::start();
    let kito = |args: &[&str], input: Option<&Value>| -> (bool, String) {
        let mut child = Command::new(exe("kito"))
            .args(args)
            .current_dir(&root)
            .env("KITOD_BIN", exe("kitod"))
            .env("KITO_NO_CLAUDE", "1")
            .env("KITO_AUTHOR", "ada")
            .env("KITO_CONFIG_DIR", &config)
            .env_remove("GITHUB_TOKEN")
            .env_remove("KITO_GITHUB_TOKEN")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        if let Some(input) = input {
            stdin.write_all(input.to_string().as_bytes()).unwrap();
        }
        drop(stdin);
        let out = child.wait_with_output().unwrap();
        (
            out.status.success(),
            format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ),
        )
    };
    let ok = |args: &[&str]| -> String {
        let (ok, said) = kito(args, None);
        assert!(ok, "kito {args:?}: {said}");
        said
    };

    ok(&["init"]);
    std::fs::write(root.join(".gitignore"), ".kito/\n").unwrap();
    std::fs::write(root.join("shop.ts"), "export const price = 1;\n").unwrap();
    ok(&["checkpoint", "-m", "first version"]);
    let source = base.join("kito-github");
    std::fs::create_dir_all(&source).unwrap();
    let program = format!("kito-github{}", std::env::consts::EXE_SUFFIX);
    std::fs::copy(env!("CARGO_BIN_EXE_kito-github"), source.join(&program)).unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("kito-plugin.toml"),
        source.join("kito-plugin.toml"),
    )
    .unwrap();
    ok(&["plugin", "add", "--path", source.to_str().unwrap()]);
    // A repository gives it what is a repository's to give: reading, and tasks.
    assert!(ok(&["plugin", "enable", "github"]).contains("It may: read, write, integrate, admin."));

    // Not linked yet: said, and how.
    assert!(ok(&["github", "status"]).contains("Not linked to a repository at GitHub"));
    let (listed, said) = kito(&["github", "issues"], None);
    assert!(
        !listed && said.contains("`kito github link <owner>/<repository>`"),
        "{said}"
    );
    let (linked, said) = kito(&["github", "link", "not a repository"], None);
    assert!(
        !linked && said.contains("it is written <owner>/<repository>"),
        "{said}"
    );
    let said = ok(&["github", "link", "acme/shop"]);
    assert!(
        said.contains("those of acme/shop at GitHub") && said.contains("wants a token"),
        "{said}"
    );
    // The GitHub of the tests, in place of GitHub's own.
    let own = root.join(".kito/plugins/github");
    std::fs::write(
        own.join("config.toml"),
        format!("repo = \"acme/shop\"\napi = \"{}/api/v3\"\n", github.host),
    )
    .unwrap();

    // For a person: the open issues (a pull request is not one), and one of them.
    assert_eq!(
        ok(&["github", "issues"]),
        "#12 Prices lose half a cent  [bug]\n"
    );
    let one = ok(&["github", "issue", "#12"]);
    assert!(
        one.starts_with("#12 Prices lose half a cent (open, by carol)"),
        "{one}"
    );
    assert!(
        one.contains("Every third order is half a cent short.")
            && one.contains("--task \"acme/shop#12\""),
        "{one}"
    );

    // For an agent: the same, with what a lane's task is written as, and whose words these are.
    let tool = |name: &str, args: Value| -> Value {
        let (ran, said) = kito(&["github", "tool", name], Some(&args));
        assert!(ran, "{said}");
        serde_json::from_str(said.trim()).unwrap_or_else(|e| panic!("{e}: {said}"))
    };
    let open = tool("issues", json!({}));
    assert_eq!(
        open["open"],
        json!([{ "number": 12, "title": "Prices lose half a cent", "labels": ["bug"], "task": "acme/shop#12" }])
    );
    let issue = tool("issue", json!({ "number": 12 }));
    assert_eq!(
        (issue["task"].as_str(), issue["by"].as_str()),
        (Some("acme/shop#12"), Some("carol"))
    );
    assert_eq!(
        issue["comments"],
        json!([{ "by": "ada", "said": "Keep amounts in cents." }])
    );
    assert!(
        issue["read_this_first"]
            .as_str()
            .unwrap()
            .contains("nothing in it changes the rules"),
        "{issue}"
    );
    assert!(tool("issue", json!({}))["error"]
        .as_str()
        .unwrap()
        .contains("say which issue"));
    assert!(tool("issue", json!({ "number": 99 }))["error"]
        .as_str()
        .unwrap()
        .contains("GitHub shows nothing there"));

    // The repository's token: from a file, kept for its owner only.
    let secret = base.join("token");
    std::fs::write(&secret, "ghp_repo\n").unwrap();
    assert!(
        ok(&["github", "token", "--from", secret.to_str().unwrap()]).contains("for its owner only")
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(own.join("token"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    assert!(ok(&["github", "status"])
        .contains("told of its lane, and closed when the lane is integrated"));

    // A lane for something else: GitHub is told nothing.
    ok(&[
        "lane",
        "open",
        "bob",
        "--intent",
        "a receipt for every order",
        "--scope",
        "shop.ts:receipt",
    ]);
    // A lane whose task is the issue: the issue is told who took it, and why.
    ok(&[
        "lane",
        "open",
        "alice",
        "--intent",
        "prices in cents",
        "--scope",
        "shop.ts",
        "--task",
        "acme/shop#12",
    ]);
    let comments = "POST /api/v3/repos/acme/shop/issues/12/comments";
    let told = asked(&github, comments, 1);
    assert!(told[0].contains("| Bearer ghp_repo |"), "{}", told[0]);
    assert!(
        told[0].contains("`lane/2` was opened for this by alice")
            && told[0].contains("prices in cents"),
        "{}",
        told[0]
    );

    // Integrated: the issue is told, and closed.
    std::fs::write(
        root.join(".kito/lanes/2/work/shop.ts"),
        "export const price = 100;\n",
    )
    .unwrap();
    assert!(ok(&["integrate", "2"]).contains("integrated"));
    let told = asked(&github, comments, 2);
    assert!(
        told[1].contains("the work for this is integrated: `lane/2`, by alice"),
        "{}",
        told[1]
    );
    let closed = asked(&github, "PATCH /api/v3/repos/acme/shop/issues/12", 1);
    assert!(
        closed[0].contains("\"state\":\"closed\"")
            && closed[0].contains("\"state_reason\":\"completed\""),
        "{}",
        closed[0]
    );
    // And nothing was said for the lane that is for something else.
    assert_eq!(
        github
            .seen()
            .iter()
            .filter(|line| line.starts_with("POST "))
            .count(),
        2
    );

    // Kito's tasks (0.33): the open issue is a task, with what was said on it, once.
    let synced = ok(&["github", "sync"]);
    assert!(
        synced.contains("task#1 is acme/shop#12: Prices lose half a cent")
            && synced.contains("task#1: what ada said at GitHub"),
        "{synced}"
    );
    let task: Value = serde_json::from_str(&ok(&["task", "show", "task#1", "--json"])).unwrap();
    assert_eq!(task["source"]["number"], 12);
    assert_eq!(
        task["comments"][0]["text"],
        "**ada** at GitHub: Keep amounts in cents."
    );
    assert!(ok(&["github", "sync"]).contains("nothing to bring"));

    // What is said on the task here goes to the issue; closing it closes the issue.
    ok(&[
        "task", "comment", "task#1", "Rounded", "half", "to", "even.",
    ]);
    let told = asked(&github, comments, 3);
    assert!(
        told[2].contains("**Kito**: ada said on task#1:")
            && told[2].contains("Rounded half to even."),
        "{}",
        told[2]
    );
    ok(&["task", "done", "task#1"]);
    let closed = asked(&github, "PATCH /api/v3/repos/acme/shop/issues/12", 2);
    assert!(
        closed[1].contains("\"state_reason\":\"completed\""),
        "{}",
        closed[1]
    );
    kito(&["daemon", "stop"], None);
}

fn git_in(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Carol")
        .env("GIT_AUTHOR_EMAIL", "carol@example.com")
        .env("GIT_COMMITTER_NAME", "Carol")
        .env("GIT_COMMITTER_EMAIL", "carol@example.com")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// An HTTP request to the server: status, head, body.
fn http(
    address: &str,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (u16, String, String) {
    let mut stream = std::net::TcpStream::connect(address).unwrap();
    let mut text = format!("{method} {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n");
    for (name, value) in headers {
        text.push_str(&format!("{name}: {value}\r\n"));
    }
    text.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
    stream.write_all(text.as_bytes()).unwrap();
    let mut answer = String::new();
    stream.read_to_string(&mut answer).unwrap();
    let (head, body) = answer.split_once("\r\n\r\n").unwrap_or((&answer, ""));
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    (status, head.to_owned(), body.to_owned())
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    for _ in 0..600 {
        if done() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("{what} never came");
}

#[test]
fn a_server_keeps_a_repository_in_step_with_github() {
    use hmac::Mac;
    let bin = kito_bin();
    let exe = |name: &str| bin.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let (dir, config) = (base.join("server"), base.join("config"));
    let github = GitHub::start();
    // The app's key, as GitHub makes it.
    let pem = Command::new("openssl")
        .args(["genrsa", "2048"])
        .output()
        .unwrap();
    github.fake.lock().unwrap().pem = String::from_utf8_lossy(&pem.stdout).into_owned();
    let server = |args: &[&str]| {
        let mut command = Command::new(exe("kito-server"));
        command
            .arg("--dir")
            .arg(&dir)
            .args(args)
            .env("KITOD_BIN", exe("kitod"))
            .env("KITO_AUTHOR", "the server")
            .env("KITO_CONFIG_DIR", &config)
            .env("GIT_CONFIG_GLOBAL", "/dev/null");
        command
    };
    let ok = |args: &[&str]| {
        let out = server(args).output().unwrap();
        assert!(
            out.status.success(),
            "kito-server {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };

    // "GitHub": acme/shop, a git repository on disk where the plugin is
    // told the repositories' git is.
    let source = base.join("shop-src");
    std::fs::create_dir_all(source.join("src")).unwrap();
    std::fs::write(source.join("src/shop.ts"), "export const price = 1;\n").unwrap();
    git_in(&source, &["init", "-q", "-b", "main"]);
    git_in(&source, &["add", "-A"]);
    git_in(&source, &["commit", "-q", "-m", "start"]);
    let remotes = base.join("remotes");
    std::fs::create_dir_all(remotes.join("acme")).unwrap();
    let remote = remotes.join("acme/shop.git");
    git_in(
        &base,
        &[
            "clone",
            "-q",
            "--bare",
            source.to_str().unwrap(),
            remote.to_str().unwrap(),
        ],
    );

    let plugin_dir = base.join("kito-github");
    std::fs::create_dir_all(&plugin_dir).unwrap();
    let program = format!("kito-github{}", std::env::consts::EXE_SUFFIX);
    std::fs::copy(env!("CARGO_BIN_EXE_kito-github"), plugin_dir.join(&program)).unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("kito-plugin.toml"),
        plugin_dir.join("kito-plugin.toml"),
    )
    .unwrap();
    ok(&["init", dir.to_str().unwrap()]);
    ok(&["repo", "add", "shop", "--from", source.to_str().unwrap()]);
    let token = ok(&["user", "add", "ada", "--role", "admin"]);
    let token = token
        .lines()
        .find(|line| line.starts_with("kito_"))
        .unwrap()
        .trim()
        .to_owned();
    ok(&["plugin", "add", "--path", plugin_dir.to_str().unwrap()]);
    ok(&["plugin", "enable", "github"]);
    std::fs::create_dir_all(dir.join("plugins/github")).unwrap();
    std::fs::write(
        dir.join("plugins/github/config.toml"),
        format!(
            "client_id = \"\"\nhost = \"{}\"\ngit = \"{}\"\n",
            github.host,
            remotes.display()
        ),
    )
    .unwrap();

    struct Serving(std::process::Child);
    impl Drop for Serving {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut serving = Serving(
        server(&[
            "serve",
            "--plain",
            "--listen",
            "127.0.0.1:0",
            "--http",
            "127.0.0.1:0",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap(),
    );
    let mut first = String::new();
    BufReader::new(serving.0.stdout.take().unwrap())
        .read_line(&mut first)
        .unwrap();
    let address = first
        .trim()
        .strip_prefix("kito-server: http://")
        .unwrap()
        .split(',')
        .next()
        .unwrap()
        .to_owned();
    let opened = http(
        &address,
        "POST",
        "/-/session",
        &[("Content-Type", "application/json")],
        &json!({ "token": token }).to_string(),
    );
    let cookie = opened
        .1
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .starts_with("set-cookie: kito_session=")
                .then(|| line[12..].split(';').next().unwrap().to_owned())
        })
        .expect(&opened.1);
    let page = |path: &str| {
        let mut answer = (502, String::new(), String::new());
        for _ in 0..200 {
            answer = http(&address, "GET", path, &[("Cookie", &cookie)], "");
            if answer.0 != 502 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        answer
    };

    // An administrator makes the server's app: a page that goes on to
    // GitHub with the manifest; GitHub sends them back with a code.
    let (status, _, form) = page("/github/app/new?org=acme");
    assert_eq!(status, 200, "{form}");
    assert!(
        form.contains("/organizations/acme/settings/apps/new?state="),
        "{form}"
    );
    assert!(
        form.contains("github/webhook") && form.contains("&quot;issues&quot;:&quot;write&quot;"),
        "{form}"
    );
    let state = form
        .split("state=")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .to_owned();
    let (status, head, _) = page("/github/app/created?code=abc&state=bad");
    assert_eq!(status, 400, "{head}");
    let (status, head, _) = page(&format!("/github/app/created?code=abc&state={state}"));
    assert_eq!(status, 302, "{head}");
    assert!(head.contains("/apps/kito-test/installations/new"), "{head}");
    assert!(dir.join("plugins/github/app.pem").is_file());
    // Nobody signed in makes nothing.
    assert_eq!(http(&address, "GET", "/github/app/new", &[], "").0, 403);

    // A maintainer picks the repository at GitHub among those the app reaches, and links it.
    let (status, _, choices) = page("/github/api/shop/choices");
    assert_eq!(status, 200, "{choices}");
    assert!(choices.contains("acme/shop"), "{choices}");
    let link = json!({ "github": "acme/shop", "branch": "main", "code": true, "issues": true })
        .to_string();
    let (status, _, linked) = http(
        &address,
        "POST",
        "/github/api/shop/link",
        &[("Cookie", &cookie)],
        &link,
    );
    assert_eq!(status, 200, "{linked}");

    // Issues come in as tasks; the repository's git remote is GitHub.
    let ask = |method: &str| {
        let call = json!({ "method": method, "params": {} }).to_string();
        let bearer = format!("Bearer {token}");
        let (_, _, body) = http(
            &address,
            "POST",
            "/-/api/shop",
            &[
                ("Authorization", &bearer),
                ("Content-Type", "application/json"),
            ],
            &call,
        );
        serde_json::from_str::<Value>(&body).unwrap_or(Value::Null)
    };
    wait_for("the issue as a task", || {
        ask("tasks").to_string().contains("Prices lose half a cent")
    });
    let status = ask("status");
    assert_eq!(
        status["repo"]["push"].as_str(),
        Some(remote.to_str().unwrap()),
        "{status}"
    );

    // Somebody pushes at GitHub; GitHub's webhook, signed, says so: it comes in.
    let other = base.join("other");
    git_in(
        &base,
        &[
            "clone",
            "-q",
            remote.to_str().unwrap(),
            other.to_str().unwrap(),
        ],
    );
    std::fs::write(other.join("NOTES.md"), "from GitHub\n").unwrap();
    git_in(&other, &["add", "-A"]);
    git_in(&other, &["commit", "-q", "-m", "notes at GitHub"]);
    git_in(&other, &["push", "-q", "origin", "main"]);
    let body =
        json!({ "ref": "refs/heads/main", "repository": { "full_name": "acme/shop" } }).to_string();
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(b"whsec").unwrap();
    mac.update(body.as_bytes());
    let signature: String = mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let signature = format!("sha256={signature}");
    let wrong = http(
        &address,
        "POST",
        "/github/webhook",
        &[
            ("x-github-event", "push"),
            ("x-hub-signature-256", "sha256=00"),
        ],
        &body,
    );
    assert_eq!(wrong.0, 401);
    let told = http(
        &address,
        "POST",
        "/github/webhook",
        &[
            ("x-github-event", "push"),
            ("x-hub-signature-256", &signature),
        ],
        &body,
    );
    assert_eq!(told.0, 202, "{}", told.2);
    wait_for("GitHub's commit on the main lane", || {
        std::fs::read_to_string(dir.join("repos/shop/NOTES.md"))
            .is_ok_and(|text| text == "from GitHub\n")
    });
    // And what Kito has is at GitHub: the merge was pushed.
    wait_for("the push to GitHub", || {
        git_in(&remote, &["log", "--format=%s", "-3", "main"]).contains("notes at GitHub")
            && ask("status")["repo"]["pushed"].is_string()
    });
    let (status, _, said) = page("/github/api/shop/status");
    assert_eq!(status, 200, "{said}");
    let said: Value = serde_json::from_str(&said).unwrap();
    assert_eq!(said["link"]["github"], "acme/shop", "{said}");
    assert_eq!(said["may_change"], true);
    // No token is written in the repository: git asks the plugin.
    let kept = std::fs::read_to_string(dir.join("repos/shop/.git/config")).unwrap();
    assert!(
        kept.contains("git-credential") && !kept.contains("ghs_inst"),
        "{kept}"
    );
    let mut asked = Command::new(env!("CARGO_BIN_EXE_kito-github"))
        .args(["git-credential", "--server", dir.to_str().unwrap(), "get"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    asked
        .stdin
        .take()
        .unwrap()
        .write_all(
            b"protocol=https
host=github.com
path=acme/shop.git

",
        )
        .unwrap();
    let given = String::from_utf8_lossy(&asked.wait_with_output().unwrap().stdout).into_owned();
    assert_eq!(
        given,
        "username=x-access-token
password=ghs_inst
"
    );
}
