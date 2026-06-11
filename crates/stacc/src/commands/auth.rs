//! `stacc auth`: GitHub token management (device-flow login, logout, status).

use serde_json::json;
use stacc_github::GitHub;

use crate::cli::{AuthAction, AuthArgs, OutputFormat};
use crate::error::Error;

/// `stacc auth`: dispatch to login / logout / status.
pub fn auth(args: &AuthArgs, format: OutputFormat) -> Result<(), Error> {
    match args.action {
        AuthAction::Login => auth_login(format),
        AuthAction::Logout => auth_logout(format),
        AuthAction::Status => auth_status(format),
    }
}

fn auth_login(format: OutputFormat) -> Result<(), Error> {
    let flow = stacc_github::DeviceFlow::default();
    // Fail fast instead of starting a device flow that GitHub will reject: this
    // build ships no registered OAuth app (the client ID is the placeholder, or
    // a stray `STACC_OAUTH_CLIENT_ID=` left it empty).
    if !flow.is_configured() {
        return Err(Error::Usage(
            "stacc auth login is unavailable: this build ships no registered GitHub \
             OAuth app. Log in with `gh` (stacc falls back to `gh auth token`) or set \
             GITHUB_TOKEN to a personal access token with `repo` scope."
                .into(),
        ));
    }
    let code = flow.request_code()?;

    // Surface the user code before polling starts, the user has to type this
    // into GitHub for the poll to ever succeed.
    match format {
        OutputFormat::Pretty => {
            println!();
            println!("To authorize stacc, open: {}", code.verification_uri);
            println!("And enter the code: {}", code.user_code);
            println!("Authorize only the app shown as \"stacc\"; never enter this code for a login you did not start.");
            println!();
            println!(
                "Waiting for authorization (expires in {} seconds)...",
                code.expires_in
            );
        }
        OutputFormat::Json => println!(
            "{}",
            json!({
                "status": "pending",
                "verification_uri": code.verification_uri,
                "user_code": code.user_code,
                "expires_in": code.expires_in,
            })
        ),
    }

    let token = flow.poll_token(&code, std::thread::sleep)?;
    stacc_github::store_token(&token)?;

    // Best-effort: confirm the token by looking up the user. A network error
    // here doesn't undo a successful login.
    let user = GitHub::new(token).current_user().ok();

    match format {
        OutputFormat::Pretty => match &user {
            Some(login) => println!("Authenticated as {login}"),
            None => println!("Token stored"),
        },
        OutputFormat::Json => println!(
            "{}",
            json!({
                "status": "authenticated",
                "user": user,
            })
        ),
    }
    Ok(())
}

fn auth_logout(format: OutputFormat) -> Result<(), Error> {
    stacc_github::clear_token()?;
    match format {
        OutputFormat::Pretty => println!("Cleared stored token"),
        OutputFormat::Json => println!("{}", json!({ "status": "logged_out" })),
    }
    Ok(())
}

// Returns `Result<(), Error>` to match the dispatcher's uniform signature even
// though no failure path is observable here.
#[allow(clippy::unnecessary_wraps)]
fn auth_status(format: OutputFormat) -> Result<(), Error> {
    let base_url = stacc_github::api_base_url();
    let env_set = stacc_github::env_token("GH_TOKEN").is_some()
        || stacc_github::env_token("GITHUB_TOKEN").is_some();
    let keyring_set = stacc_github::keychain_token().is_some();
    // gh is only a usable source for github.com; on a custom host `from_env`
    // never consults it, so don't report it as a source there.
    let gh_set =
        stacc_github::is_github_dot_com(&base_url) && stacc_github::gh_token().is_some();
    let source = if env_set {
        "env"
    } else if keyring_set {
        "keyring"
    } else if gh_set {
        "gh"
    } else {
        "none"
    };

    // Only verify by hitting the API if we actually have a token.
    let user = if source == "none" {
        None
    } else {
        GitHub::from_env().ok().and_then(|gh| gh.current_user().ok())
    };

    match format {
        OutputFormat::Pretty => match source {
            "env" => {
                println!("Authenticated via environment variable");
                if let Some(u) = &user {
                    println!("Logged in as {u}");
                }
                if keyring_set {
                    println!("(a stored token also exists; the env var takes precedence)");
                }
            }
            "keyring" => {
                println!("Authenticated via stored token");
                if let Some(u) = &user {
                    println!("Logged in as {u}");
                }
            }
            "gh" => {
                println!("Authenticated via the gh CLI");
                if let Some(u) = &user {
                    println!("Logged in as {u}");
                }
            }
            _ => println!("Not authenticated. Run `stacc auth login`, set GITHUB_TOKEN, or log in with `gh`."),
        },
        OutputFormat::Json => println!(
            "{}",
            json!({
                "source": source,
                "user": user,
                "env_set": env_set,
                "keyring_set": keyring_set,
                "gh_set": gh_set,
            })
        ),
    }
    Ok(())
}
