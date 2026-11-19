use std::{
    fs::{File, Metadata},
    io::Read,
};

use secrecy::SecretString;
use thiserror::Error;
use tracing::{event, instrument, Level};
use zeroize::Zeroizing;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    read_env_file()?;
    let username = match std::env::var("WIKIDOT_USERNAME") {
        Err(error) => {
            event!(Level::ERROR, %error);
            return Err(error.into());
        }
        Ok(username) => username,
    };
    let password = read_pw_file()?;

    Ok(())
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
