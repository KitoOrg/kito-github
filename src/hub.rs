//! On a Kito server (Kito 0.39): repositories kept in step with GitHub.
//!
//! The plugin serves pages under `/github` (`kito-github serve-pages`),
//! which is where:
//!
//! - an administrator makes the server's GitHub App in one click
//!   (GitHub's manifest flow): the app's key and webhook secret are kept
//!   in `plugins/github/` of the server, for its user only;
//! - GitHub tells the server what changed (`/github/webhook`, signed with
//!   the webhook secret);
//! - the web client reads and changes a repository's link
//!   (`/github/api/<repository>/…`), for its maintainers.
//!
//! The process also checks every few minutes, for a server GitHub cannot
//! reach, and follows each linked repository's events to tell GitHub what
//! happened to the tasks of its issues.
//!
//! Code: the repository's git remote is the GitHub repository, and git is
//! given the app's token for it by this program (`kito-github
//! git-credential`), asked when it needs one: no token is written in the
//! repository. Kito itself pulls (GitHub's commits come in through a lane,
//! merged and checked) and pushes the main lane and the permanent lanes'
//! branches (`git_sync`, ADR 0020 of Kito).

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const APP_FILE: &str = "app.toml";
const KEY_FILE: &str = "app.pem";
const LINKS_FILE: &str = "links.json";
/// How often everything is checked, whatever GitHub says.
const EVERY: Duration = Duration::from_secs(300);
/// How often the repositories' events are read.
const EVENTS_EVERY: Duration = Duration::from_secs(10);

fn said(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

fn own_dir() -> Result<PathBuf, String> {
    let dir =
        said("KITO_SERVER_DIR").ok_or("KITO_SERVER_DIR is not set: this runs on a Kito server")?;
    Ok(PathBuf::from(dir).join("plugins").join("github"))
}

/// Written for the server's user only.
fn write_private(path: &std::path::Path, text: &str) -> Result<(), String> {
    use std::io::Write as _;
    let said = |e: std::io::Error| format!("{}: {e}", path.display());
    std::fs::create_dir_all(path.parent().unwrap_or(path)).map_err(said)?;
    let staged = path.with_extension("new");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options
        .open(&staged)
        .and_then(|mut out| out.write_all(text.as_bytes()))
        .map_err(said)?;
    std::fs::rename(&staged, path).map_err(said)
}

// ------------------------------------------------------------- the app

/// The server's GitHub App, as GitHub made it.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct App {
    pub id: u64,
    pub slug: String,
    pub owner: String,
    pub html_url: String,
    pub webhook_secret: String,
}

impl App {
    pub fn load() -> Option<App> {
        let text = std::fs::read_to_string(own_dir().ok()?.join(APP_FILE)).ok()?;
        toml::from_str(&text).ok().filter(|app: &App| app.id > 0)
    }

    fn save(&self, key: &str) -> Result<(), String> {
        let dir = own_dir()?;
        write_private(&dir.join(KEY_FILE), key)?;
        write_private(
            &dir.join(APP_FILE),
            &toml::to_string(self).map_err(|e| e.to_string())?,
        )
    }

    /// What the app says it is, signed with its key: for ten minutes.
    fn jwt(&self) -> Result<String, String> {
        let key =
            std::fs::read(own_dir()?.join(KEY_FILE)).map_err(|e| format!("the app's key: {e}"))?;
        let key = jsonwebtoken::EncodingKey::from_rsa_pem(&key)
            .map_err(|e| format!("the app's key: {e}"))?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        let claims =
            json!({ "iat": now.saturating_sub(60), "exp": now + 540, "iss": self.id.to_string() });
        jsonwebtoken::encode(
            &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256),
            &claims,
            &key,
        )
        .map_err(|e| format!("signing for the app: {e}"))
    }
}

fn api() -> String {
    crate::Config::load()
        .map(|config| config.api())
        .unwrap_or_else(|_| "https://api.github.com".to_owned())
}

fn host() -> String {
    crate::Config::load()
        .map(|config| config.host())
        .unwrap_or_else(|_| "https://github.com".to_owned())
}

/// Asks GitHub as the app (`Some(jwt)`) or with a token.
fn github(
    method: &str,
    url: &str,
    bearer: &str,
    body: Option<&Value>,
) -> Result<(u16, Value), String> {
    let request = crate::agent()
        .request(method, url)
        .set("Accept", "application/vnd.github+json")
        .set("Authorization", &format!("Bearer {bearer}"));
    crate::answer_of(match body {
        Some(body) => request.send_json(body.clone()),
        None => request.call(),
    })
}

/// Tokens of the app's installations, kept while they last.
static TOKENS: Mutex<BTreeMap<u64, (String, Instant)>> = Mutex::new(BTreeMap::new());

/// A token for what an installation of the app may reach: an hour long,
/// taken again ten minutes before it ends.
pub fn installation_token(installation: u64) -> Result<String, String> {
    if let Some((token, until)) = TOKENS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&installation)
    {
        if Instant::now() < *until {
            return Ok(token.clone());
        }
    }
    let app = App::load()
        .ok_or("this server has no GitHub App: an admin makes one on the Plugins page")?;
    let url = format!("{}/app/installations/{installation}/access_tokens", api());
    match github("POST", &url, &app.jwt()?, None)? {
        (201, answer) => {
            let token = answer["token"]
                .as_str()
                .ok_or("GitHub gave no token")?
                .to_owned();
            let until = Instant::now() + Duration::from_secs(50 * 60);
            TOKENS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(installation, (token.clone(), until));
            Ok(token)
        }
        (status, answer) => Err(format!(
            "GitHub gave the app no token (status {status}): {}",
            answer["message"].as_str().unwrap_or_default()
        )),
    }
}

/// The installation of the app that reaches `owner/name`.
fn installation_of(repo: &str) -> Result<u64, String> {
    let app = App::load().ok_or("this server has no GitHub App")?;
    match github("GET", &format!("{}/repos/{repo}/installation", api()), &app.jwt()?, None)? {
        (200, found) => found["id"].as_u64().ok_or_else(|| "GitHub named no installation".to_owned()),
        (404, _) => Err(format!(
            "the GitHub App of this server is not installed where {repo} is: install it there ({}/apps/{}/installations/new)",
            host(),
            app.slug
        )),
        (status, answer) => Err(format!("GitHub answered {status}: {}", answer["message"].as_str().unwrap_or_default())),
    }
}

/// Every repository the app may reach, in every installation.
fn reachable() -> Result<Vec<Value>, String> {
    let app = App::load().ok_or("this server has no GitHub App")?;
    let (status, installations) = github(
        "GET",
        &format!("{}/app/installations", api()),
        &app.jwt()?,
        None,
    )?;
    if status != 200 {
        return Err(format!(
            "GitHub did not list the app's installations (status {status})"
        ));
    }
    let mut found = Vec::new();
    for installation in installations.as_array().into_iter().flatten() {
        let Some(id) = installation["id"].as_u64() else {
            continue;
        };
        let token = installation_token(id)?;
        let mut page = 1;
        loop {
            let url = format!(
                "{}/installation/repositories?per_page=100&page={page}",
                api()
            );
            let (status, listed) = github("GET", &url, &token, None)?;
            let repos = listed["repositories"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            if status != 200 {
                break;
            }
            found.extend(repos.iter().map(|repo| {
                json!({
                    "name": repo["full_name"],
                    "private": repo["private"],
                    "branch": repo["default_branch"],
                    "installation": id,
                })
            }));
            if repos.len() < 100 {
                break;
            }
            page += 1;
        }
    }
    Ok(found)
}

// ------------------------------------------------------------- the links

/// A repository of the server kept in step with one at GitHub.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Link {
    /// `owner/name`.
    pub github: String,
    pub installation: u64,
    /// Code: pulled and pushed.
    pub code: bool,
    /// Issues as tasks, and tasks told at GitHub.
    pub issues: bool,
    /// The branch the main lane is, at GitHub.
    pub branch: String,
    pub last_ms: u64,
    pub last_error: Option<String>,
    /// What the last sync did.
    pub said: Vec<String>,
    /// The repository's events read up to here, as the daemon said it.
    pub events_after: Option<Value>,
}

static LINKS: Mutex<()> = Mutex::new(());

fn links() -> BTreeMap<String, Link> {
    own_dir()
        .ok()
        .and_then(|dir| std::fs::read_to_string(dir.join(LINKS_FILE)).ok())
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Changes the links, one change at a time.
fn change_links<T>(work: impl FnOnce(&mut BTreeMap<String, Link>) -> T) -> Result<T, String> {
    let _held = LINKS.lock().unwrap_or_else(|e| e.into_inner());
    let mut all = links();
    let done = work(&mut all);
    let text = serde_json::to_string_pretty(&all).map_err(|e| e.to_string())?;
    write_private(&own_dir()?.join(LINKS_FILE), &text)?;
    Ok(done)
}

// ------------------------------------------------------------- the server

/// Asks the server about one of its repositories, as this plugin (ADR
/// 0020 of Kito): with what an administrator gave it.
pub fn kito(repo: &str, method: &str, params: Value) -> Result<Value, String> {
    let api = said("KITO_SERVER_API")
        .ok_or("the server did not say where it is (KITO_SERVER_API): Kito 0.39 or later")?;
    let secret = said("KITO_PAGES_SECRET").ok_or("the server gave no secret")?;
    let asked = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(120))
        .build()
        .post(&format!("{}/-/api/{repo}", api.trim_end_matches('/')))
        .set("x-kito-plugin", &secret)
        .send_json(json!({ "method": method, "params": params }));
    match asked {
        Ok(answer) => answer.into_json().map_err(|e| e.to_string()),
        Err(ureq::Error::Status(_, answer)) => {
            let said: Value = answer.into_json().unwrap_or(Value::Null);
            Err(said["error"]
                .as_str()
                .unwrap_or("the server refused")
                .to_owned())
        }
        Err(e) => Err(format!("the server did not answer: {e}")),
    }
}

fn server_dir() -> Result<PathBuf, String> {
    said("KITO_SERVER_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| "KITO_SERVER_DIR is not set".to_owned())
}

/// The repository's git remote is the one at GitHub, and git asks this
/// program for a token when it needs one.
fn ready_for_git(repo: &str, link: &Link) -> Result<(), String> {
    let git = crate::Config::load()
        .ok()
        .and_then(|config| config.git)
        .unwrap_or_else(host);
    let url = format!("{}/{}.git", git.trim_end_matches('/'), link.github);
    let status = kito(repo, "status", json!({}))?;
    if status["repo"]["push"].as_str() != Some(url.as_str()) {
        kito(repo, "configure", json!({ "push": url }))?;
    }
    let root = server_dir()?.join("repos").join(repo);
    let program = std::env::current_exe().map_err(|e| e.to_string())?;
    let helper = format!(
        "!'{}' git-credential --server '{}'",
        program.display(),
        server_dir()?.display()
    );
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(args)
            .output()
            .map_err(|e| format!("git: {e}"))
    };
    let key = format!("credential.{}.helper", host());
    git(&["config", "--local", "--replace-all", &key, &helper])?;
    git(&["config", "--local", "credential.useHttpPath", "true"])?;
    if !link.branch.is_empty() {
        // The branch Kito advances is the one at GitHub.
        let head = git(&["symbolic-ref", "--short", "HEAD"])?;
        let now = String::from_utf8_lossy(&head.stdout).trim().to_owned();
        if now != link.branch && !now.is_empty() {
            let _ = git(&["branch", "-m", &now, &link.branch]);
        }
    }
    Ok(())
}

/// Keeps one repository in step, now: code, then issues.
pub fn sync_one(repo: &str) -> Result<Vec<String>, String> {
    let Some(link) = links().get(repo).cloned() else {
        return Err(format!("`{repo}` is not linked to a repository at GitHub"));
    };
    let mut said = Vec::new();
    let mut failed = None;
    if link.code {
        match ready_for_git(repo, &link).and_then(|()| kito(repo, "git_sync", json!({}))) {
            Ok(answer) => {
                if let Some(error) = answer["push_error"].as_str() {
                    failed = Some(format!("the last push: {error}"));
                }
                if let Some(lane) = answer["pull_waiting"].as_str() {
                    said.push(format!("GitHub's commits wait in {lane} for a decision"));
                }
                said.push(format!("code: pulled from and pushed to {}", link.github));
            }
            Err(why) => failed = Some(why),
        }
    }
    if link.issues && failed.is_none() {
        let context = issue_context(repo, &link)?;
        match crate::issues::on_server(context, crate::issues::sync_here) {
            Ok(done) if done.is_empty() => said.push("issues: as the tasks are".to_owned()),
            Ok(done) => said.extend(done),
            Err(why) => failed = Some(why),
        }
    }
    change_links(|all| {
        if let Some(link) = all.get_mut(repo) {
            link.last_ms = now_ms();
            link.last_error = failed.clone();
            link.said = said.clone();
        }
    })?;
    match failed {
        Some(why) => Err(why),
        None => Ok(said),
    }
}

fn issue_context(repo: &str, link: &Link) -> Result<crate::issues::OnServer, String> {
    Ok(crate::issues::OnServer {
        kito: repo.to_owned(),
        github: link.github.clone(),
        api: api(),
        token: installation_token(link.installation)?,
    })
}

/// What happened in the linked repositories since last read: tasks of
/// issues done or dropped, lanes of issues opened or integrated, told at
/// GitHub.
fn follow_events() {
    for (repo, link) in links() {
        if !link.issues {
            continue;
        }
        let mut asked = json!({ "wait_ms": 0 });
        if let Some(after) = &link.events_after {
            asked["after"] = json!(after);
        }
        let Ok(answer) = kito(&repo, "events", asked) else {
            continue;
        };
        let first = link.events_after.is_none();
        if !first {
            if let Ok(context) = issue_context(&repo, &link) {
                for event in answer["events"].as_array().into_iter().flatten() {
                    let _ = crate::issues::on_server(context.clone(), || {
                        crate::issues::handle_event(event)
                    });
                }
            }
        }
        let next = answer["next"].clone();
        if !next.is_null() && Some(&next) != link.events_after.as_ref() {
            let _ = change_links(|all| {
                if let Some(link) = all.get_mut(&repo) {
                    link.events_after = Some(next);
                }
            });
        }
    }
}

// ------------------------------------------------------------- git

/// `kito-github git-credential --server <dir> get`: git asks for what to
/// say to GitHub for a repository; the answer is the app's token for it.
pub fn git_credential(args: &[&str]) -> Result<(), String> {
    if let Some(at) = args.iter().position(|arg| *arg == "--server") {
        if let Some(dir) = args.get(at + 1) {
            std::env::set_var("KITO_SERVER_DIR", dir);
        }
    }
    if args.last() != Some(&"get") {
        // `store` and `erase`: nothing is kept.
        return Ok(());
    }
    let mut asked = String::new();
    std::io::stdin()
        .read_to_string(&mut asked)
        .map_err(|e| e.to_string())?;
    let field = |name: &str| {
        asked
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{name}=")).map(str::to_owned))
    };
    let path = field("path").unwrap_or_default();
    let repo = path.trim_end_matches(".git").trim_matches('/').to_owned();
    let Some(link) = links()
        .into_values()
        .find(|link| link.github.eq_ignore_ascii_case(&repo))
    else {
        return Ok(());
    };
    let token = installation_token(link.installation)?;
    println!("username=x-access-token\npassword={token}");
    Ok(())
}

// ------------------------------------------------------------- pages

struct Asked {
    method: String,
    path: String,
    query: BTreeMap<String, String>,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let (mut out, mut at) = (Vec::new(), 0);
    while at < bytes.len() {
        let hex = bytes
            .get(at + 1..at + 3)
            .and_then(|hex| std::str::from_utf8(hex).ok());
        match (
            bytes[at],
            hex.and_then(|hex| u8::from_str_radix(hex, 16).ok()),
        ) {
            (b'%', Some(byte)) => {
                out.push(byte);
                at += 2;
            }
            (b'+', _) => out.push(b' '),
            (byte, _) => out.push(byte),
        }
        at += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

type Reply = (u16, &'static str, String, Vec<(&'static str, String)>);

fn json_reply(status: u16, value: Value) -> Reply {
    (status, "application/json", value.to_string(), Vec::new())
}

fn redirect(to: &str) -> Reply {
    (
        302,
        "text/plain",
        String::new(),
        vec![("Location", to.to_owned())],
    )
}

/// The server's address, as the browser that asks names it.
fn base(asked: &Asked) -> String {
    let host = asked.headers.get("host").cloned().unwrap_or_default();
    let local =
        host.starts_with("127.") || host.starts_with("localhost") || host.starts_with("[::1]");
    format!("{}://{host}", if local { "http" } else { "https" })
}

fn admin(asked: &Asked) -> bool {
    matches!(
        asked.headers.get("x-kito-role").map(String::as_str),
        Some("admin" | "owner")
    )
}

/// States of the app being made, kept while this runs.
static STATES: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn new_app(asked: &Asked) -> Reply {
    if !admin(asked) {
        return (
            403,
            "text/plain",
            "for the server's admins".to_owned(),
            Vec::new(),
        );
    }
    let base = base(asked);
    let state = crate::random_state().unwrap_or_default();
    STATES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(state.clone());
    let short = asked.headers.get("host").cloned().unwrap_or_default();
    let name: String = format!("Kito {short}").chars().take(34).collect();
    let manifest = json!({
        "name": name,
        "url": base,
        "hook_attributes": { "url": format!("{base}/github/webhook"), "active": true },
        "redirect_url": format!("{base}/github/app/created"),
        "callback_urls": [format!("{base}/-/sign-in/github/callback")],
        "setup_url": format!("{base}/github/app/installed"),
        "setup_on_update": true,
        "public": false,
        "request_oauth_on_install": false,
        "default_permissions": { "contents": "write", "issues": "write", "metadata": "read" },
        "default_events": ["push", "issues", "issue_comment"],
    });
    let org = asked.query.get("org").filter(|org| {
        !org.is_empty() && org.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    });
    let action = match org {
        Some(org) => format!(
            "{}/organizations/{org}/settings/apps/new?state={state}",
            host()
        ),
        None => format!("{}/settings/apps/new?state={state}", host()),
    };
    let page = format!(
        "<!doctype html><meta charset=utf-8><title>Making the GitHub App…</title>\
         <body style=\"font:14px system-ui;margin:40px\"><p>Taking you to GitHub to make this server's app…</p>\
         <form id=f method=post action=\"{}\"><input type=hidden name=manifest value=\"{}\">\
         <button>Go on to GitHub</button></form><script>document.getElementById('f').submit()</script>",
        html_escape(&action),
        html_escape(&manifest.to_string())
    );
    (200, "text/html; charset=utf-8", page, Vec::new())
}

fn app_created(asked: &Asked) -> Reply {
    if !admin(asked) {
        return (
            403,
            "text/plain",
            "for the server's admins".to_owned(),
            Vec::new(),
        );
    }
    let state = asked.query.get("state").cloned().unwrap_or_default();
    let known = {
        let mut states = STATES.lock().unwrap_or_else(|e| e.into_inner());
        let found = states.iter().position(|kept| *kept == state);
        found.map(|at| states.remove(at)).is_some()
    };
    let Some(code) = asked.query.get("code").filter(|_| known) else {
        return (
            400,
            "text/plain",
            "this is not the app this server asked for: start again from the Plugins page"
                .to_owned(),
            Vec::new(),
        );
    };
    let made = crate::agent()
        .post(&format!("{}/app-manifests/{code}/conversions", api()))
        .set("Accept", "application/vnd.github+json")
        .call();
    let (status, app) = match crate::answer_of(made) {
        Ok(answer) => answer,
        Err(why) => return (502, "text/plain", why, Vec::new()),
    };
    if status != 201 {
        return (
            502,
            "text/plain",
            format!("GitHub did not make the app (status {status})"),
            Vec::new(),
        );
    }
    let kept = App {
        id: app["id"].as_u64().unwrap_or(0),
        slug: app["slug"].as_str().unwrap_or_default().to_owned(),
        owner: app["owner"]["login"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        html_url: app["html_url"].as_str().unwrap_or_default().to_owned(),
        webhook_secret: app["webhook_secret"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
    };
    if let Err(why) = kept.save(app["pem"].as_str().unwrap_or_default()) {
        return (500, "text/plain", why, Vec::new());
    }
    // People sign in with the same app.
    if let Ok(mut config) = crate::Config::load() {
        config.client_id = app["client_id"].as_str().unwrap_or_default().to_owned();
        config.client_secret = app["client_secret"].as_str().map(str::to_owned);
        let _ = config.save();
    }
    redirect(&format!("{}/apps/{}/installations/new", host(), kept.slug))
}

/// May `who` change the link of `repo`: a maintainer of it, or an
/// administrator of the server.
fn maintains(asked: &Asked, repo: &str) -> bool {
    if admin(asked) {
        return true;
    }
    let Some(who) = asked.headers.get("x-kito-user") else {
        return false;
    };
    kito(repo, "server.members", json!({}))
        .ok()
        .and_then(|members| {
            members
                .as_array()
                .cloned()
                .or_else(|| members["members"].as_array().cloned())
        })
        .is_some_and(|members| {
            members.iter().any(|member| {
                member["name"].as_str() == Some(who)
                    && matches!(member["role"].as_str(), Some("maintain"))
            })
        })
}

/// The server's app as the pages show it: what it is called, whose it
/// is, and where it is installed on repositories.
fn app_json(app: &App) -> Value {
    json!({
        "slug": app.slug, "owner": app.owner, "url": app.html_url,
        "install": format!("{}/apps/{}/installations/new", host(), app.slug),
    })
}

/// Whether the server has its app, for whoever is signed in, and whether
/// they may make one: the page that makes it offers it once.
fn app_info(asked: &Asked) -> Reply {
    if !asked.headers.contains_key("x-kito-user") {
        return json_reply(401, json!({ "error": "nobody is signed in" }));
    }
    json_reply(
        200,
        json!({ "app": App::load().as_ref().map(app_json), "may_create": admin(asked) }),
    )
}

fn repo_api(asked: &Asked, repo: &str, what: &str) -> Reply {
    if !asked.headers.contains_key("x-kito-user") {
        return json_reply(401, json!({ "error": "nobody is signed in" }));
    }
    let app = App::load();
    match (asked.method.as_str(), what) {
        ("GET", "status") => {
            let link = links().get(repo).cloned();
            json_reply(
                200,
                json!({
                    "app": app.as_ref().map(app_json),
                    "link": link.map(|link| json!({
                        "github": link.github, "code": link.code, "issues": link.issues, "branch": link.branch,
                        "last_ms": link.last_ms, "last_error": link.last_error, "said": link.said,
                    })),
                    "may_change": maintains(asked, repo),
                    "permanent": kito(repo, "lanes", json!({})).ok().map(|lanes| lanes.as_array().cloned().unwrap_or_default()
                        .into_iter().filter(|lane| lane["permanent"] == true || lane["name"].is_string())
                        .filter_map(|lane| lane["name"].as_str().map(str::to_owned)).collect::<Vec<_>>()).unwrap_or_default(),
                }),
            )
        }
        ("GET", "choices") => match reachable() {
            Ok(found) => json_reply(200, json!({ "repositories": found })),
            Err(why) => json_reply(400, json!({ "error": why })),
        },
        ("POST", _) if !maintains(asked, repo) => json_reply(
            403,
            json!({ "error": "only a maintainer of this repository, or an admin, links it" }),
        ),
        ("POST", "link") => {
            let said: Value = serde_json::from_slice(&asked.body).unwrap_or(Value::Null);
            let github = said["github"]
                .as_str()
                .unwrap_or_default()
                .trim()
                .to_owned();
            let fits = github.split_once('/').is_some_and(|(owner, name)| {
                !owner.is_empty() && !name.is_empty() && !name.contains('/')
            });
            if !fits {
                return json_reply(
                    400,
                    json!({ "error": "a repository at GitHub is written <owner>/<name>" }),
                );
            }
            let installation = match installation_of(&github) {
                Ok(id) => id,
                Err(why) => return json_reply(400, json!({ "error": why })),
            };
            let branch = said["branch"].as_str().unwrap_or("main").trim().to_owned();
            let done = change_links(|all| {
                let link = all.entry(repo.to_owned()).or_default();
                let moved = link.github != github;
                link.github = github.clone();
                link.installation = installation;
                link.code = said["code"].as_bool().unwrap_or(true);
                link.issues = said["issues"].as_bool().unwrap_or(true);
                link.branch = if branch.is_empty() {
                    "main".to_owned()
                } else {
                    branch
                };
                if moved {
                    link.events_after = None;
                }
            });
            if let Err(why) = done {
                return json_reply(500, json!({ "error": why }));
            }
            let repo = repo.to_owned();
            std::thread::spawn(move || {
                let _ = sync_one(&repo);
            });
            json_reply(200, json!({ "linked": github }))
        }
        ("POST", "unlink") => {
            let _ = change_links(|all| all.remove(repo));
            json_reply(200, json!({ "unlinked": true }))
        }
        ("POST", "sync") => match sync_one(repo) {
            Ok(said) => json_reply(200, json!({ "said": said })),
            Err(why) => json_reply(400, json!({ "error": why })),
        },
        _ => json_reply(404, json!({ "error": "nothing here" })),
    }
}

/// A webhook of GitHub: checked against the app's secret, then the linked
/// repositories it is about are kept in step.
fn webhook(asked: &Asked) -> Reply {
    use hmac::Mac;
    let Some(app) = App::load() else {
        return json_reply(404, json!({ "error": "no app" }));
    };
    let sent = asked
        .headers
        .get("x-hub-signature-256")
        .cloned()
        .unwrap_or_default();
    let Ok(mut mac) = hmac::Hmac::<sha2::Sha256>::new_from_slice(app.webhook_secret.as_bytes())
    else {
        return json_reply(500, json!({ "error": "no secret" }));
    };
    mac.update(&asked.body);
    let expected: String = mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let given = sent.strip_prefix("sha256=").unwrap_or_default();
    let same = given.len() == expected.len()
        && given
            .bytes()
            .zip(expected.bytes())
            .fold(0u8, |diff, (a, b)| diff | (a ^ b))
            == 0;
    if !same {
        return json_reply(401, json!({ "error": "not signed by this server's app" }));
    }
    let event: Value = serde_json::from_slice(&asked.body).unwrap_or(Value::Null);
    let kind = asked
        .headers
        .get("x-github-event")
        .cloned()
        .unwrap_or_default();
    let full = event["repository"]["full_name"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let mine: Vec<String> = links()
        .into_iter()
        .filter(|(_, link)| link.github.eq_ignore_ascii_case(&full))
        .filter(|(_, link)| match kind.as_str() {
            "push" => link.code,
            "issues" | "issue_comment" => link.issues,
            _ => false,
        })
        .map(|(repo, _)| repo)
        .collect();
    let count = mine.len();
    std::thread::spawn(move || {
        for repo in mine {
            let _ = sync_one(&repo);
        }
    });
    json_reply(202, json!({ "repositories": count }))
}

fn answer(asked: &Asked) -> Reply {
    let path = asked.path.trim_start_matches("/github");
    match (asked.method.as_str(), path) {
        ("POST", "/webhook") => webhook(asked),
        ("GET", "/app") => app_info(asked),
        ("GET", "/app/new") => new_app(asked),
        ("GET", "/app/created") => app_created(asked),
        ("GET", "/app/installed") => redirect("/#/admin"),
        (_, rest) if rest.starts_with("/api/") => {
            let mut parts = rest["/api/".len()..].splitn(2, '/');
            let (repo, what) = (
                parts.next().unwrap_or_default(),
                parts.next().unwrap_or_default(),
            );
            if repo.is_empty() || repo.contains("..") {
                return json_reply(404, json!({ "error": "no such repository" }));
            }
            repo_api(asked, &decode(repo), what)
        }
        _ => (404, "text/plain", "nothing here".to_owned(), Vec::new()),
    }
}

/// `kito-github serve-pages`: the pages, and keeping the linked
/// repositories in step, until the server closes the standard input.
pub fn serve() -> Result<(), String> {
    std::thread::spawn(|| {
        let mut gone = Vec::new();
        let _ = std::io::stdin().read_to_end(&mut gone);
        std::process::exit(0);
    });
    let listen = said("KITO_PAGES_LISTEN").ok_or("the server said nowhere to listen")?;
    let secret = said("KITO_PAGES_SECRET").ok_or("the server gave no secret")?;
    let server = tiny_http::Server::http(&listen).map_err(|e| format!("{listen}: {e}"))?;
    // By itself: everything every few minutes, the events more often.
    std::thread::spawn(|| {
        let mut last = Instant::now() - EVERY;
        loop {
            follow_events();
            if last.elapsed() >= EVERY {
                last = Instant::now();
                for repo in links().keys() {
                    let _ = sync_one(repo);
                }
            }
            std::thread::sleep(EVENTS_EVERY);
        }
    });
    for mut request in server.incoming_requests() {
        let mut headers = BTreeMap::new();
        for header in request.headers() {
            headers.insert(
                header.field.as_str().as_str().to_ascii_lowercase(),
                header.value.as_str().to_owned(),
            );
        }
        // Only what the server passes on.
        if headers.get("x-kito-secret") != Some(&secret) {
            let _ = request.respond(
                tiny_http::Response::from_string("not from the server").with_status_code(403),
            );
            continue;
        }
        let url = request.url().to_owned();
        let (path, query) = url.split_once('?').unwrap_or((&url, ""));
        let query = query
            .split('&')
            .filter(|pair| !pair.is_empty())
            .map(|pair| pair.split_once('=').unwrap_or((pair, "")))
            .map(|(key, value)| (decode(key), decode(value)))
            .collect();
        let mut body = Vec::new();
        let _ = request
            .as_reader()
            .take(5 * 1024 * 1024)
            .read_to_end(&mut body);
        let asked = Asked {
            method: request.method().as_str().to_owned(),
            path: path.to_owned(),
            query,
            headers,
            body,
        };
        let (status, kind, text, extra) = answer(&asked);
        let mut response = tiny_http::Response::from_string(text).with_status_code(status);
        if let Ok(header) = tiny_http::Header::from_bytes("Content-Type", kind) {
            response.add_header(header);
        }
        for (name, value) in extra {
            if let Ok(header) = tiny_http::Header::from_bytes(name, value) {
                response.add_header(header);
            }
        }
        let _ = request.respond(response);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asked(headers: &[(&str, &str)], body: &[u8]) -> Asked {
        Asked {
            method: "POST".into(),
            path: "/github/webhook".into(),
            query: BTreeMap::new(),
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: body.to_vec(),
        }
    }

    #[test]
    fn a_webhook_counts_only_signed_with_the_app_s_secret_and_addresses_decode() {
        use hmac::Mac;
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("KITO_SERVER_DIR", tmp.path());
        let app = App {
            id: 7,
            slug: "kito-x".into(),
            webhook_secret: "s3cret".into(),
            ..Default::default()
        };
        app.save("not a key").unwrap();
        let body = br#"{"repository":{"full_name":"acme/shop"}}"#;
        let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(b"s3cret").unwrap();
        mac.update(body);
        let good: String = mac
            .finalize()
            .into_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let signed = format!("sha256={good}");
        let (status, ..) = webhook(&asked(
            &[("x-hub-signature-256", &signed), ("x-github-event", "push")],
            body,
        ));
        assert_eq!(status, 202);
        let (status, ..) = webhook(&asked(
            &[
                ("x-hub-signature-256", "sha256=00"),
                ("x-github-event", "push"),
            ],
            body,
        ));
        assert_eq!(status, 401);
        let (status, ..) = webhook(&asked(&[("x-github-event", "push")], body));
        assert_eq!(status, 401);
        // Kept for the server's user only.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(tmp.path().join("plugins/github/app.pem"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0);
        }
        assert_eq!(decode("acme%2Fshop+x%zz"), "acme/shop x%zz");
    }
}
