use std::{
    fs::{File, Metadata},
    io::Read,
    path::PathBuf,
};

use clap::{Parser, Subcommand};
use scraper::{Html, Selector};
use secrecy::{ExposeSecret, SecretString};
use thiserror::Error;
use tracing::{event, instrument, Level};
use ureq::{http::Response, Agent, Body, Cookie};
use zeroize::Zeroizing;

mod actions;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Parser)]
struct Cli {
    /// The Wikidot site where the action must be performed (e.g. `scp-wiki`).
    site: String,
    /// The action to be performed.
    #[command(subcommand)]
    action: Action,
    /// Always say 'yes' to confirmation prompts.
    #[arg(short, long)]
    yes: bool,
}

#[derive(Subcommand)]
enum Action {
    Replace {
        /// The path to the file containing the slugs of the pages whose contents must be replaced (one slug per line inside the file).
        page_list_file: PathBuf,
        /// The path to the file containing the new contents for those pages.
        content_file: PathBuf,
        /// If this flag is specified, then the target pages will be put into the `unlisted` category, and copies with no history will be made to show the new contents.
        #[arg(long)]
        discard_history: bool,
        /// If nonexistent pages are mentioned to this action, it will not create them unless this flag is specified.
        #[arg(long)]
        create: bool,
        /// Page edition locks will not be taken by force unless this flag is specified.
        #[arg(short, long)]
        forceful: bool,
        #[arg(short, long)]
        message: Option<String>,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Cli::parse();

    read_env_file()?;
    let username = read_username()?;
    let password = read_pw_file()?;

    let agent = ureq::agent();

    let auth_response = auth(&agent, &username, &password)?;
    let auth_response_body_str = read_body(auth_response.into_body())?;
    let auth_response_body = Html::parse_document(&auth_response_body_str);

    let auth_err_selector = Selector::parse("h2.title.error.alert.alert-danger")
        .expect("the auth error CSS selector should be valid");
    if auth_response_body
        .select(&auth_err_selector)
        .find(|elem| {
            elem.inner_html()
                .contains("The login and password do not match.")
        })
        .is_some()
    {
        let error = AuthError::WrongPassword;
        event!(Level::ERROR, %error);
        return Err(error.into());
    }

    extend_csrf_token(&agent, &args.site)?;

    event!(Level::INFO, "connection succeeded");

    match args.action {
        Action::Replace {
            page_list_file,
            content_file,
            discard_history,
            create,
            forceful,
            message,
        } => actions::replace(
            &agent,
            &args.site,
            &username,
            actions::EditOptions::new(page_list_file, create, forceful, message),
            actions::ReplaceOptions::new(content_file, discard_history),
            args.yes,
        )?,
    }

    Ok(())
}

#[derive(Error, Debug)]
enum AuthError {
    #[error("wrong password")]
    WrongPassword,
    #[error("Wikidot did not transmit a CSRF token")]
    NoCsrfToken,
}

#[instrument(err)]
fn read_env_file() -> Result<()> {
    if let Err(error) = dotenvy::dotenv() {
        if error.not_found() {
            event!(Level::WARN, "no .env file found");
        } else {
            return Err(error.into());
        }
    }
    Ok(())
}

#[instrument(err)]
fn read_username() -> Result<String> {
    std::env::var("WIKIDOT_USERNAME").map_err(Into::into)
}

#[derive(Error, Debug)]
enum PasswordFileError {
    #[error("the password file is not a regular file")]
    NotRegularFile,
    #[error("the password file has an insecure Unix mode")]
    InsecureMode,
    #[error("the password file is not owned by the current user")]
    WrongUser,
}

#[instrument(err)]
fn read_pw_file() -> Result<SecretString> {
    let pw_file_path = std::env::var("WIKIDOT_PASSWORD_FILE")?;
    let mut file = File::open(pw_file_path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(PasswordFileError::NotRegularFile.into());
    }
    check_pw_file_on_unix(&metadata)?;
    let mut pw = Zeroizing::new(String::with_capacity(metadata.len() as usize));
    file.read_to_string(&mut pw)?;
    let trimming_len = pw.trim_end_matches(['\r', '\n']).len(); // `trim_end_matches` returns a slice without copying anything
    pw.truncate(trimming_len); // trim in place
    Ok(SecretString::from(pw.as_str()))
}

fn check_pw_file_on_unix(metadata: &Metadata) -> Result<()> {
    cfg_select! {
        unix => {
            use std::os::unix::fs::MetadataExt;
            if metadata.mode() & 0o077 != 0 {
                Err(PasswordFileError::InsecureMode.into())
            } else if metadata.uid() != rustix::process::getuid().as_raw() {
                Err(PasswordFileError::WrongUser.into())
            } else {
                Ok(())
            }
        }
        _ => {
            event!(Level::WARN, "password file security checks only work on Unix");
            Ok(())
        }
    }
}

#[instrument(err, skip(agent))]
fn auth(agent: &Agent, username: &str, password: &SecretString) -> Result<Response<Body>> {
    agent
        .post("https://www.wikidot.com/default--flow/login__LoginPopupScreen")
        .send_form([
            ("login", username),
            ("password", password.expose_secret()),
            ("action", "Login2Action"),
            ("event", "login"),
        ])
        .map_err(Into::into)
}

#[instrument(err)]
fn read_body(mut body: Body) -> Result<String> {
    body.read_to_string().map_err(Into::into)
}

fn get_csrf_token(agent: &Agent) -> Result<String> {
    let cookie_jar = agent.cookie_jar_lock();

    let wikidot_token7 = cookie_jar
        .get("www.wikidot.com", "/", "wikidot_token7")
        .ok_or(AuthError::NoCsrfToken)?
        .value()
        .to_owned();

    cookie_jar.release();
    Ok(wikidot_token7)
}

/// Wikidot transmits the CSRF token as a cookie that is only valid for www.wikidot.com.
/// This function makes it also valid for the relevant Wikidot site.
#[instrument(err, skip(agent))]
fn extend_csrf_token(agent: &Agent, site: &str) -> Result<()> {
    let wikidot_token7 = get_csrf_token(agent)?;

    let mut cookie_jar = agent.cookie_jar_lock();

    let site_uri = format!("https://{}.wikidot.com", site).parse()?;
    let new_wikidot_token7 =
        Cookie::parse(format!("wikidot_token7={}", wikidot_token7), &site_uri)?;
    cookie_jar.insert(new_wikidot_token7, &site_uri)?;

    cookie_jar.release();
    Ok(())
}
