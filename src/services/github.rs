//! GitHub: signing in so git can push and pull over HTTPS, and making
//! repositories to publish to.
//!
//! This runs in the server, next to git. Signing in hands the token to git's
//! own credential helper there (the macOS keychain, Git Credential Manager on
//! Windows, or the GitHub CLI when git is set up to use it), so every push to
//! github.com uses it: the Git window's and your terminals'. What shows as
//! signed in is what git has for github.com, checked with GitHub.
//!
//! Ways to sign in:
//! - In the browser, with a one-time code (GitHub's device flow): through
//!   XHarness's GitHub app when the build has one (`XHARNESS_GITHUB_CLIENT_ID`
//!   at build time or in the server's environment), else through the GitHub
//!   CLI (`gh`) when it's installed.
//! - As an account the GitHub CLI is already signed in to.
//! - With a personal access token.
//!
//! Requests go through `curl`. Tokens reach curl, git and gh on stdin, never
//! in arguments, which anyone on the machine can list.

use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Write},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::services::paths::user_path;
use crate::services::rpc::{handle_call, route};

const HOST: &str = "github.com";
const API: &str = "https://api.github.com";
/// What a token can do: repositories and their workflows, and seeing which
/// organizations you're in (to make repositories there).
const SCOPES: &str = "repo workflow read:org";
/// Where to make a token by hand, with those scopes ticked.
pub const NEW_TOKEN_URL: &str =
    "https://github.com/settings/tokens/new?description=XHarness&scopes=repo,workflow,read:org";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub login: String,
    pub name: Option<String>,
    /// Organizations it's in.
    pub orgs: Vec<String>,
}

/// What git has for github.com.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SignedIn {
    No,
    Yes(Account),
    /// A token for this login that GitHub turns down (revoked or expired).
    Rejected(String),
    /// Credentials for this login; GitHub couldn't be asked about them.
    Unchecked { login: String, error: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Accounts {
    pub signed_in: SignedIn,
    /// Where git keeps the token ("the macOS keychain").
    pub stored_in: String,
    /// Accounts the GitHub CLI is signed in to.
    pub cli: Vec<String>,
    /// Whether signing in in the browser works on this server.
    pub browser: bool,
}

/// A browser sign-in waiting for its code to be entered on GitHub.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceCode {
    pub id: u64,
    pub code: String,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SignIn {
    Waiting,
    Done(Account),
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Repository {
    pub clone_url: String,
    pub html_url: String,
}

// Routed: these run in the server (see services/rpc.rs) --------------------------

pub fn accounts() -> Result<Accounts, String> {
    route("github.accounts", (), accounts_here)
}

/// Starts a sign-in in the browser; poll it with [`sign_in_status`].
pub fn begin_sign_in() -> Result<DeviceCode, String> {
    route("github.begin_sign_in", (), begin_sign_in_here)
}

pub fn sign_in_status(id: u64) -> Result<SignIn, String> {
    route("github.sign_in_status", (id,), || sign_in_status_here(id))
}

pub fn cancel_sign_in(id: u64) -> Result<(), String> {
    route("github.cancel_sign_in", (id,), || cancel_sign_in_here(id))
}

pub fn sign_in_with_token(token: &str) -> Result<Account, String> {
    route("github.sign_in_with_token", (token,), || sign_in_with_token_here(token))
}

/// Signs git in as an account the GitHub CLI is signed in to.
pub fn sign_in_with_cli(login: &str) -> Result<Account, String> {
    route("github.sign_in_with_cli", (login,), || sign_in_with_cli_here(login))
}

pub fn sign_out() -> Result<(), String> {
    route("github.sign_out", (), sign_out_here)
}

/// Makes an empty repository, for you or in `org`.
pub fn create_repository(org: Option<&str>, name: &str, description: &str, private: bool) -> Result<Repository, String> {
    route("github.create_repository", (org, name, description, private), || {
        create_repository_here(org, name, description, private)
    })
}

/// Runs a `github.*` call for the app (see `rpc.rs`).
pub fn dispatch(method: &str, params: Value) -> Option<Result<Value, String>> {
    Some(match method {
        "github.accounts" => handle_call(params, |_: Value| accounts()),
        "github.begin_sign_in" => handle_call(params, |_: Value| begin_sign_in()),
        "github.sign_in_status" => handle_call(params, |(id,): (u64,)| sign_in_status(id)),
        "github.cancel_sign_in" => handle_call(params, |(id,): (u64,)| cancel_sign_in(id)),
        "github.sign_in_with_token" => handle_call(params, |(token,): (String,)| sign_in_with_token(&token)),
        "github.sign_in_with_cli" => handle_call(params, |(login,): (String,)| sign_in_with_cli(&login)),
        "github.sign_out" => handle_call(params, |_: Value| sign_out()),
        "github.create_repository" => handle_call(
            params,
            |(org, name, description, private): (Option<String>, String, String, bool)| {
                create_repository(org.as_deref(), &name, &description, private)
            },
        ),
        _ => return None,
    })
}

/// The web page for a GitHub remote's URL (`https://github.com/o/r.git`,
/// `git@github.com:o/r.git`, `ssh://git@github.com/o/r`), if it is one.
pub fn web_url(remote: &str) -> Option<String> {
    let rest = remote
        .strip_prefix("https://github.com/")
        .or_else(|| remote.strip_prefix("http://github.com/"))
        .or_else(|| remote.strip_prefix("git@github.com:"))
        .or_else(|| remote.strip_prefix("ssh://git@github.com/"))
        .or_else(|| {
            // https://user@github.com/o/r
            let after = remote.strip_prefix("https://")?;
            let (user, path) = after.split_once('@')?;
            (!user.contains('/')).then_some(path)?.strip_prefix("github.com/")
        })?;
    let path = rest.trim_end_matches('/').trim_end_matches(".git");
    let mut parts = path.split('/');
    let (owner, repo) = (parts.next()?, parts.next()?);
    (!owner.is_empty() && !repo.is_empty() && parts.next().is_none()).then(|| format!("https://github.com/{owner}/{repo}"))
}

/// Whether git reaches this remote over SSH (keys, not the GitHub sign-in).
pub fn is_ssh(remote: &str) -> bool {
    remote.starts_with("ssh://") || (!remote.contains("://") && remote.contains('@') && remote.contains(':'))
}

// Accounts -----------------------------------------------------------------------------

fn accounts_here() -> Result<Accounts, String> {
    let signed_in = match saved() {
        None => SignedIn::No,
        Some((login, token)) => match user(&token) {
            Ok(account) => SignedIn::Yes(account),
            Err(Failure::Rejected) => SignedIn::Rejected(login),
            Err(Failure::Other(error)) => SignedIn::Unchecked { login, error },
        },
    };
    let cli = cli_accounts();
    Ok(Accounts {
        signed_in,
        stored_in: stored_in(helper().as_deref()),
        browser: client_id().is_some() || gh_installed(),
        cli,
    })
}

/// The account a token is for, and its organizations.
fn user(token: &str) -> Result<Account, Failure> {
    let reply = request("GET", &format!("{API}/user"), Some(token), None).map_err(Failure::Other)?;
    match reply.status {
        200 => {}
        401 => return Err(Failure::Rejected),
        _ => return Err(Failure::Other(reply.message())),
    }
    let login = reply.body["login"].as_str().unwrap_or_default().to_string();
    if login.is_empty() {
        return Err(Failure::Other("GitHub didn't say who the token is for.".into()));
    }
    let orgs = request("GET", &format!("{API}/user/orgs?per_page=100"), Some(token), None)
        .ok()
        .filter(|reply| reply.status == 200)
        .and_then(|reply| reply.body.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|org| org["login"].as_str().map(String::from))
        .collect();
    Ok(Account { login, name: reply.body["name"].as_str().filter(|n| !n.is_empty()).map(String::from), orgs })
}

enum Failure {
    Rejected,
    Other(String),
}

/// Checks a token, then hands it to git (or to gh, when git asks gh).
fn sign_in_with_token_here(token: &str) -> Result<Account, String> {
    let token = token.trim();
    if token.is_empty() {
        return Err("Paste a token first.".into());
    }
    let account = match user(token) {
        Ok(account) => account,
        Err(Failure::Rejected) => return Err("GitHub turned the token down. Check it's whole and not expired.".into()),
        Err(Failure::Other(error)) => return Err(error),
    };
    store(&account.login, token)?;
    Ok(account)
}

fn sign_in_with_cli_here(login: &str) -> Result<Account, String> {
    if uses_gh_helper() {
        // Git asks gh, which answers with its active account.
        gh(&["auth", "switch", "--hostname", HOST, "--user", login], None)?;
        return match user(&gh(&["auth", "token", "--hostname", HOST, "--user", login], None)?) {
            Ok(account) => Ok(account),
            Err(Failure::Rejected) => Err(format!("GitHub turned down the GitHub CLI's token for {login}. Run `gh auth refresh`.")),
            Err(Failure::Other(error)) => Err(error),
        };
    }
    let token = gh(&["auth", "token", "--hostname", HOST, "--user", login], None)?;
    sign_in_with_token_here(&token)
}

fn sign_out_here() -> Result<(), String> {
    let Some((login, _)) = saved() else {
        return Ok(());
    };
    if uses_gh_helper() {
        return gh(&["auth", "logout", "--hostname", HOST, "--user", &login], None).map(|_| ());
    }
    credential("reject", &[("protocol", "https"), ("host", HOST), ("username", &login)])?;
    // Some helpers keep one per username; a second, older one would answer next.
    if saved().is_some_and(|(other, _)| other == login) {
        credential("reject", &[("protocol", "https"), ("host", HOST)])?;
    }
    Ok(())
}

// Git's credentials ----------------------------------------------------------------------

/// The login and token git has for github.com.
fn saved() -> Option<(String, String)> {
    let out = credential("fill", &[("protocol", "https"), ("host", HOST)]).ok()?;
    let field = |name: &str| out.lines().find_map(|line| line.strip_prefix(name)?.strip_prefix('=')).map(String::from);
    Some((field("username").unwrap_or_default(), field("password").filter(|p| !p.is_empty())?))
}

/// Hands the token to git's helper, replacing what it had.
fn store(login: &str, token: &str) -> Result<(), String> {
    if uses_gh_helper() {
        // gh keeps tokens itself (and ignores git's "store").
        gh(&["auth", "login", "--hostname", HOST, "--git-protocol", "https", "--with-token"], Some(token))?;
        return Ok(());
    }
    if helper().is_none() {
        let default = if cfg!(target_os = "macos") {
            "osxkeychain"
        } else if cfg!(windows) {
            "manager"
        } else {
            // Plain text in ~/.git-credentials (only you can read it).
            "store"
        };
        git_config(&["--global", "credential.https://github.com.helper", default])?;
    }
    credential("reject", &[("protocol", "https"), ("host", HOST)]).ok();
    credential("approve", &[("protocol", "https"), ("host", HOST), ("username", login), ("password", token)])?;
    Ok(())
}

/// `git credential <action>`, never prompting.
fn credential(action: &str, fields: &[(&str, &str)]) -> Result<String, String> {
    let mut input = String::new();
    for (key, value) in fields {
        input.push_str(&format!("{key}={value}\n"));
    }
    input.push('\n');
    let mut command = git_command();
    command.args(["credential", action]);
    run(command, Some(&input), "git credential")
}

/// The helper git uses for github.com, if any.
fn helper() -> Option<String> {
    let mut command = git_command();
    command.args(["config", "--get-urlmatch", "credential.helper", "https://github.com"]);
    run(command, None, "git config").ok().map(|out| out.trim().to_string()).filter(|h| !h.is_empty())
}

fn uses_gh_helper() -> bool {
    helper().is_some_and(|helper| helper.contains("gh auth git-credential"))
}

fn stored_in(helper: Option<&str>) -> String {
    let Some(helper) = helper else {
        return if cfg!(target_os = "macos") {
            "the macOS keychain".into()
        } else if cfg!(windows) {
            "Git Credential Manager".into()
        } else {
            "~/.git-credentials".into()
        };
    };
    let name = helper.trim_start_matches('!').split_whitespace().next().unwrap_or(helper);
    let name = name.rsplit(['/', '\\']).next().unwrap_or(name).trim_start_matches("git-credential-");
    match name {
        "osxkeychain" => "the macOS keychain".into(),
        "manager" | "manager-core" | "git-credential-manager" => "Git Credential Manager".into(),
        "gh" => "the GitHub CLI".into(),
        "store" => "~/.git-credentials".into(),
        "cache" => "git's memory cache (for a while)".into(),
        "libsecret" | "gnome-keyring" => "the system keyring".into(),
        "wincred" => "Windows Credential Manager".into(),
        other => format!("git's “{other}” helper"),
    }
}

fn git_config(args: &[&str]) -> Result<(), String> {
    let mut command = git_command();
    command.arg("config").args(args);
    run(command, None, "git config").map(|_| ())
}

fn git_command() -> Command {
    let mut command = crate::services::command("git");
    command
        .env("PATH", user_path())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .env_remove("GIT_ASKPASS")
        .env_remove("SSH_ASKPASS");
    // Outside any repository: only your own settings count.
    if let Some(home) = std::env::home_dir() {
        command.current_dir(home);
    }
    command
}

// The GitHub CLI -------------------------------------------------------------------------

fn gh_installed() -> bool {
    std::env::split_paths(user_path()).any(|dir| dir.join(if cfg!(windows) { "gh.exe" } else { "gh" }).is_file())
}

fn gh_command() -> Command {
    let mut command = crate::services::command("gh");
    command.env("PATH", user_path()).env("GH_PROMPT_DISABLED", "1").env("NO_COLOR", "1");
    command
}

fn gh(args: &[&str], input: Option<&str>) -> Result<String, String> {
    if !gh_installed() {
        return Err("The GitHub CLI (gh) isn't installed.".into());
    }
    let mut command = gh_command();
    command.args(args);
    run(command, input, "gh").map(|out| out.trim().to_string())
}

/// The accounts gh is signed in to on github.com, the active one first.
fn cli_accounts() -> Vec<String> {
    let Ok(out) = gh(&["auth", "status", "--hostname", HOST, "--json", "hosts"], None) else {
        return Vec::new();
    };
    let Ok(status) = serde_json::from_str::<Value>(&out) else {
        return Vec::new();
    };
    let mut accounts: Vec<(bool, String)> = status["hosts"][HOST]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|account| account["state"] == "success")
        .filter_map(|account| Some((account["active"] == true, account["login"].as_str()?.to_string())))
        .collect();
    accounts.sort_by_key(|(active, _)| !active);
    accounts.into_iter().map(|(_, login)| login).collect()
}

// Signing in in the browser ------------------------------------------------------------

struct Pending {
    state: Mutex<SignIn>,
    cancelled: AtomicBool,
    /// gh, while it waits (cancelling ends it).
    child: Mutex<Option<Child>>,
}

fn pending() -> &'static Mutex<HashMap<u64, Arc<Pending>>> {
    static PENDING: OnceLock<Mutex<HashMap<u64, Arc<Pending>>>> = OnceLock::new();
    PENDING.get_or_init(Default::default)
}

fn client_id() -> Option<String> {
    std::env::var("XHARNESS_GITHUB_CLIENT_ID")
        .ok()
        .or(option_env!("XHARNESS_GITHUB_CLIENT_ID").map(String::from))
        .filter(|id| !id.trim().is_empty())
}

fn begin_sign_in_here() -> Result<DeviceCode, String> {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let job = Arc::new(Pending { state: Mutex::new(SignIn::Waiting), cancelled: AtomicBool::new(false), child: Mutex::new(None) });
    let (code, url) = match client_id() {
        Some(client_id) => device_flow(client_id, job.clone())?,
        None if gh_installed() => gh_flow(job.clone())?,
        None => return Err("Signing in in the browser needs the GitHub CLI (gh) on the server. Use a token instead.".into()),
    };
    pending().lock().unwrap().insert(id, job);
    Ok(DeviceCode { id, code, url })
}

fn sign_in_status_here(id: u64) -> Result<SignIn, String> {
    let mut pending = pending().lock().unwrap();
    let job = pending.get(&id).ok_or("That sign-in isn't going on any more.")?.clone();
    let state = job.state.lock().unwrap().clone();
    if state != SignIn::Waiting {
        pending.remove(&id);
    }
    Ok(state)
}

fn cancel_sign_in_here(id: u64) -> Result<(), String> {
    if let Some(job) = pending().lock().unwrap().remove(&id) {
        job.cancelled.store(true, Ordering::Relaxed);
        if let Some(mut child) = job.child.lock().unwrap().take() {
            child.kill().ok();
            child.wait().ok();
        }
    }
    Ok(())
}

/// Finishes a sign-in with the token GitHub gave.
fn finish(job: &Pending, token: Result<String, String>) {
    let result = token.and_then(|token| sign_in_with_token_here(&token));
    *job.state.lock().unwrap() = match result {
        Ok(account) => SignIn::Done(account),
        Err(error) => SignIn::Failed(error),
    };
}

/// GitHub's device flow, with XHarness's own app.
fn device_flow(client_id: String, job: Arc<Pending>) -> Result<(String, String), String> {
    let form = format!("client_id={}&scope={}", encode(&client_id), encode(SCOPES));
    let reply = request_form("https://github.com/login/device/code", &form)?;
    if reply.status != 200 || reply.body.get("device_code").is_none() {
        return Err(reply.message());
    }
    let field = |name: &str| reply.body[name].as_str().unwrap_or_default().to_string();
    let (device_code, code, url) = (field("device_code"), field("user_code"), field("verification_uri"));
    let mut interval = reply.body["interval"].as_u64().unwrap_or(5).max(1);
    let deadline = Instant::now() + Duration::from_secs(reply.body["expires_in"].as_u64().unwrap_or(900));
    thread::spawn(move || {
        let form = format!(
            "client_id={}&device_code={}&grant_type=urn:ietf:params:oauth:grant-type:device_code",
            encode(&client_id),
            encode(&device_code)
        );
        let mut failures = 0;
        loop {
            thread::sleep(Duration::from_secs(interval));
            if job.cancelled.load(Ordering::Relaxed) {
                return;
            }
            if Instant::now() > deadline {
                return finish(&job, Err("The code expired. Sign in again.".into()));
            }
            let reply = match request_form("https://github.com/login/oauth/access_token", &form) {
                Ok(reply) => reply,
                Err(error) => {
                    failures += 1;
                    if failures > 5 {
                        return finish(&job, Err(error));
                    }
                    continue;
                }
            };
            if let Some(token) = reply.body["access_token"].as_str() {
                return finish(&job, Ok(token.to_string()));
            }
            match reply.body["error"].as_str().unwrap_or_default() {
                "authorization_pending" => {}
                "slow_down" => interval = reply.body["interval"].as_u64().unwrap_or(interval + 5),
                "expired_token" => return finish(&job, Err("The code expired. Sign in again.".into())),
                "access_denied" => return finish(&job, Err("Sign-in was cancelled on GitHub.".into())),
                _ => return finish(&job, Err(reply.message())),
            }
        }
    });
    Ok((code, url))
}

/// `gh auth login --web`: without a terminal, it prints the code and the
/// page, then waits for you to finish in the browser.
fn gh_flow(job: Arc<Pending>) -> Result<(String, String), String> {
    let mut command = gh_command();
    command
        .args(["auth", "login", "--web", "--hostname", HOST, "--git-protocol", "https", "--skip-ssh-key"])
        .args(["--scopes", "workflow"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    // The app opens the page (on your machine, which may not be the server).
    #[cfg(unix)]
    command.env("GH_BROWSER", "true").env("BROWSER", "true");
    let mut child = command.spawn().map_err(|error| format!("Can't run gh: {error}."))?;
    let stderr = child.stderr.take().unwrap();
    let (lines, read) = mpsc::channel::<String>();
    thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if lines.send(line).is_err() {
                break;
            }
        }
    });
    let (mut code, mut url, mut said) = (None, None, Vec::new());
    let deadline = Instant::now() + Duration::from_secs(20);
    while code.is_none() || url.is_none() {
        let Ok(line) = read.recv_timeout(deadline.saturating_duration_since(Instant::now())) else {
            child.kill().ok();
            child.wait().ok();
            let said = said.join(" ");
            return Err(if said.is_empty() { "gh didn't give a code.".into() } else { format!("gh: {said}") });
        };
        if let Some(found) = line.split("one-time code:").nth(1) {
            code = Some(found.trim().to_string());
        }
        if let Some(start) = line.find("https://") {
            url = Some(line[start..].split_whitespace().next().unwrap_or_default().to_string());
        }
        said.push(line.trim().trim_start_matches(['!', '✓', 'X']).trim().to_string());
    }
    *job.child.lock().unwrap() = Some(child);
    thread::spawn(move || {
        let mut said = Vec::new();
        let status = loop {
            while let Ok(line) = read.try_recv() {
                said.push(line.trim().trim_start_matches(['!', '✓', 'X']).trim().to_string());
            }
            let polled = match job.child.lock().unwrap().as_mut() {
                // Cancelled: gh was ended.
                None => return,
                Some(child) => child.try_wait(),
            };
            match polled {
                Ok(Some(status)) => break status,
                Ok(None) => thread::sleep(Duration::from_millis(250)),
                Err(error) => return finish(&job, Err(error.to_string())),
            }
        };
        job.child.lock().unwrap().take();
        while let Ok(line) = read.try_recv() {
            said.push(line.trim().to_string());
        }
        if !status.success() {
            let said: Vec<_> = said.into_iter().filter(|line| !line.is_empty()).collect();
            return finish(&job, Err(said.last().cloned().unwrap_or_else(|| "gh couldn't sign in.".into())));
        }
        // Whoever was just signed in is gh's active account now.
        if uses_gh_helper() {
            let token = gh(&["auth", "token", "--hostname", HOST], None);
            *job.state.lock().unwrap() = match token.and_then(|token| user(&token).map_err(|_| "GitHub turned the new token down.".to_string())) {
                Ok(account) => SignIn::Done(account),
                Err(error) => SignIn::Failed(error),
            };
            return;
        }
        finish(&job, gh(&["auth", "token", "--hostname", HOST], None));
    });
    Ok((code.unwrap(), url.unwrap()))
}

// Repositories -----------------------------------------------------------------------------

fn create_repository_here(org: Option<&str>, name: &str, description: &str, private: bool) -> Result<Repository, String> {
    let (_, token) = saved().ok_or("Sign in to GitHub first.")?;
    let name = name.trim();
    if name.is_empty() {
        return Err("Give the repository a name.".into());
    }
    let url = match org {
        Some(org) => format!("{API}/orgs/{}/repos", encode(org)),
        None => format!("{API}/user/repos"),
    };
    let body = json!({ "name": name, "description": description.trim(), "private": private });
    let reply = request("POST", &url, Some(&token), Some(&body))?;
    if reply.status != 201 {
        return Err(reply.message());
    }
    let field = |name: &str| reply.body[name].as_str().unwrap_or_default().to_string();
    Ok(Repository { clone_url: field("clone_url"), html_url: field("html_url") })
}

// HTTP, through curl ----------------------------------------------------------------------

struct Reply {
    status: u16,
    body: Value,
}

impl Reply {
    /// GitHub's explanation, for showing.
    fn message(&self) -> String {
        let detail = self.body["errors"]
            .as_array()
            .and_then(|errors| errors.first())
            .and_then(|error| error["message"].as_str().or_else(|| error["code"].as_str()));
        let message = self.body["error_description"].as_str().or(self.body["message"].as_str());
        match (message, detail) {
            (Some(message), Some(detail)) => format!("GitHub: {message} ({detail})."),
            (Some(message), None) => format!("GitHub: {}.", message.trim_end_matches('.')),
            (None, _) => format!("GitHub answered {}.", self.status),
        }
    }
}

/// A GitHub API request.
fn request(method: &str, url: &str, token: Option<&str>, body: Option<&Value>) -> Result<Reply, String> {
    let mut config = vec![
        format!("url = {}", quote(url)),
        format!("request = {}", quote(method)),
        format!("header = {}", quote("Accept: application/vnd.github+json")),
        format!("header = {}", quote("X-GitHub-Api-Version: 2022-11-28")),
    ];
    if let Some(token) = token {
        config.push(format!("header = {}", quote(&format!("Authorization: Bearer {}", token.trim()))));
    }
    if let Some(body) = body {
        config.push(format!("header = {}", quote("Content-Type: application/json")));
        config.push(format!("data-binary = {}", quote(&body.to_string())));
    }
    curl(&config)
}

/// A form POST to github.com's sign-in pages.
fn request_form(url: &str, form: &str) -> Result<Reply, String> {
    curl(&[
        format!("url = {}", quote(url)),
        format!("header = {}", quote("Accept: application/json")),
        format!("data = {}", quote(form)),
    ])
}

/// Runs curl with `config` on its stdin.
fn curl(config: &[String]) -> Result<Reply, String> {
    let mut command = crate::services::command("curl");
    command
        .env("PATH", user_path())
        .args(["--silent", "--show-error", "--max-time", "30", "--user-agent", "XHarness"])
        .args(["--write-out", "\n%{http_code}", "--config", "-"]);
    let out = run(command, Some(&(config.join("\n") + "\n")), "curl")
        .map_err(|error| format!("Can't reach GitHub: {}", error.trim_start_matches("curl: ").trim_start_matches(|c: char| c == '(' || c.is_ascii_digit() || c == ')' || c == ' ')))?;
    let (body, status) = out.rsplit_once('\n').unwrap_or(("", out.as_str()));
    let status = status.trim().parse().map_err(|_| "GitHub's answer didn't make sense.".to_string())?;
    Ok(Reply { status, body: serde_json::from_str(body).unwrap_or(Value::Null) })
}

/// A string for curl's config file.
fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n"))
}

/// Percent-encodes for a form or a URL path.
fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (byte as char).to_string(),
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

/// Runs a command, `input` on its stdin; its output, or what it said on
/// stderr.
fn run(mut command: Command, input: Option<&str>, name: &str) -> Result<String, String> {
    command.stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| format!("Can't run {name}: {error}."))?;
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        stdin.write_all(input.as_bytes()).ok();
    }
    let output = child.wait_with_output().map_err(|error| error.to_string())?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let said = stderr
        .lines()
        .map(|line| line.trim().trim_start_matches("fatal: ").trim_start_matches("error: "))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    Err(if said.is_empty() { format!("{name} failed.") } else { said })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_remotes_have_web_pages() {
        for remote in [
            "https://github.com/astralyx/xharness.git",
            "https://github.com/astralyx/xharness",
            "https://nik@github.com/astralyx/xharness.git",
            "git@github.com:astralyx/xharness.git",
            "ssh://git@github.com/astralyx/xharness",
        ] {
            assert_eq!(web_url(remote).as_deref(), Some("https://github.com/astralyx/xharness"), "{remote}");
        }
        assert_eq!(web_url("https://gitlab.com/a/b.git"), None);
        assert_eq!(web_url("https://github.com/only-owner"), None);
        assert!(is_ssh("git@github.com:a/b.git"));
        assert!(is_ssh("ssh://git@host/a"));
        assert!(!is_ssh("https://github.com/a/b.git"));
        assert!(!is_ssh("/Users/me/repo.git"));
    }

    #[test]
    fn helpers_are_named() {
        assert_eq!(stored_in(Some("osxkeychain")), "the macOS keychain");
        assert_eq!(stored_in(Some("!/opt/homebrew/bin/gh auth git-credential")), "the GitHub CLI");
        assert_eq!(stored_in(Some("/usr/local/share/gcm-core/git-credential-manager")), "Git Credential Manager");
        assert_eq!(stored_in(Some("store --file ~/.creds")), "~/.git-credentials");
    }

    /// Asks this machine's git and GitHub who's signed in (read-only):
    /// `cargo test -- --ignored live_accounts --nocapture`.
    #[test]
    #[ignore = "talks to GitHub"]
    fn live_accounts() {
        let accounts = accounts().unwrap();
        println!("{accounts:#?}");
    }

    /// Starts a browser sign-in through gh (with a throwaway gh config) and
    /// cancels it: `cargo test -- --ignored live_browser_sign_in --nocapture`.
    #[test]
    #[ignore = "talks to GitHub"]
    fn live_browser_sign_in_starts_and_cancels() {
        let config = std::env::temp_dir().join(format!("xh-gh-{}", std::process::id()));
        std::fs::create_dir_all(&config).unwrap();
        // SAFETY: the only test that sets it; run on its own.
        unsafe { std::env::set_var("GH_CONFIG_DIR", &config) };
        let code = begin_sign_in().unwrap();
        println!("{code:?}");
        assert!(code.code.len() == 9 && code.code.as_bytes()[4] == b'-', "{}", code.code);
        assert!(code.url.starts_with("https://github.com/login/device"));
        assert_eq!(sign_in_status(code.id).unwrap(), SignIn::Waiting);
        cancel_sign_in(code.id).unwrap();
        assert!(sign_in_status(code.id).is_err(), "gone once cancelled");
        std::fs::remove_dir_all(&config).ok();
    }

    #[test]
    fn curl_config_and_forms_are_escaped() {
        assert_eq!(quote(r#"{"a":"b\"c"}"#), r#""{\"a\":\"b\\\"c\"}""#);
        assert_eq!(encode("repo workflow read:org"), "repo%20workflow%20read%3Aorg");
    }
}
