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
    let code = flow.request_code()?;

    // Surface the user code before polling starts, the user has to type this
    // into GitHub for the poll to ever succeed.
    match format {
        OutputFormat::Pretty => {
            println!();
            println!("To authorize stacc, open: {}", code.verification_uri);
            println!("And enter the code: {}", code.user_code);
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
    let env_set =
        std::env::var("GITHUB_TOKEN").is_ok() || std::env::var("GH_TOKEN").is_ok();
    let keyring_set = stacc_github::load_token().is_some();
    let source = if env_set {
        "env"
    } else if keyring_set {
        "keyring"
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
                println!("Authenticated via GITHUB_TOKEN");
                if let Some(u) = &user {
                    println!("Logged in as {u}");
                }
                if keyring_set {
                    println!("(a stored token also exists; env var takes precedence)");
                }
            }
            "keyring" => {
                println!("Authenticated via stored token");
                if let Some(u) = &user {
                    println!("Logged in as {u}");
                }
            }
            _ => println!("Not authenticated. Run `stacc auth login` or set GITHUB_TOKEN."),
        },
        OutputFormat::Json => println!(
            "{}",
            json!({
                "source": source,
                "user": user,
                "env_set": env_set,
                "keyring_set": keyring_set,
            })
        ),
    }
    Ok(())
}
