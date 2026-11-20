use std::{
    collections::HashSet,
    fs::File,
    io::{BufRead, BufReader, IsTerminal},
    path::Path,
};

use indicatif::{ProgressIterator, ProgressStyle};
use thiserror::Error;
use tracing::{event, instrument, Level};
use ureq::http::StatusCode;

#[derive(Error, Debug)]
enum ArgError {
    #[error("not a valid slug")]
    InvalidSlug(String),
}

#[derive(Error, Debug)]
enum UserAborted {
    #[error("the user aborted the execution")]
    Manually,
    #[error("no interactive tty and -y flag not provided")]
    NoInteractiveTty,
}

#[instrument(err)]
pub fn replace(
    site: &str,
    page_list_file_path: &Path,
    content_file_path: &Path,
    discard_history: bool,
    create: bool,
    yes: bool,
) -> crate::Result<()> {
    let page_slugs = slugs_from_file(page_list_file_path)?;
    let PageExistenceBins {
        existent,
        nonexistent,
    } = check_existence(site, page_slugs)?;

    let info_msg = if create && !nonexistent.is_empty() {
        format!(
            "this action will edit {} pages and create {} more",
            existent.len(),
            nonexistent.len()
        )
    } else if !nonexistent.is_empty() {
        format!(
            "this action will edit {} pages. Among the mentioned pages, {} more do not exist yet, and will not be created",
            existent.len(),
            nonexistent.len()
        )
    } else {
        format!("this action will edit {} pages", existent.len())
    };

    if !yes {
        let mut prompt_msg = info_msg.clone();
        if let Some(first) = prompt_msg.get_mut(..1) {
            first.make_ascii_uppercase();
        }
        prompt_msg += &(if !create && !nonexistent.is_empty() {
            ". Run this with the `--create` flag if you want to create them"
        } else {
            ""
        }
        .to_owned()
            + ".\n\nRun this as is? [y/N]");
        eprint!("{}", prompt_msg);

        let mut stdin = std::io::stdin().lock();
        if stdin.is_terminal() {
            let mut input = String::new();
            stdin.read_line(&mut input)?;
            if input.trim() != "y" {
                return Err(UserAborted::Manually.into());
            }
        } else {
            return Err(UserAborted::NoInteractiveTty.into());
        }
    }

    event!(Level::INFO, "{info_msg}");

    if discard_history {
        for existent_page in existent {
            move_to_category(site, &existent_page, "unlisted")?;
        }
    }

    todo!("do the actual replacing");

    Ok(())
}

#[instrument(err)]
fn slugs_from_file(page_list_file_path: &Path) -> crate::Result<HashSet<String>> {
    let page_list_file = File::open(page_list_file_path)?;
    let page_list_file_reader = BufReader::new(page_list_file);
    let page_slugs: HashSet<String> = page_list_file_reader
        .lines()
        .map_while(Result::ok)
        .filter_map(|mut line| {
            if line.is_empty() {
                return None;
            }

            let line_in_file = line.clone();
            wikidot_normalize::normalize(&mut line);
            Some(if line == line_in_file {
                Ok(line)
            } else {
                Err(ArgError::InvalidSlug(line_in_file))
            })
        })
        .collect::<Result<_, _>>()?;
    event!(Level::INFO, "{} pages mentioned", page_slugs.len());
    Ok(page_slugs)
}

struct PageExistenceBins {
    existent: HashSet<String>,
    nonexistent: HashSet<String>,
}

#[derive(Error, Debug)]
enum ReadError {
    #[error("Wikidot responded with an unexpected status")]
    UnexpectedStatus(StatusCode),
}

#[instrument(err, skip(page_slugs))]
fn check_existence(site: &str, page_slugs: HashSet<String>) -> crate::Result<PageExistenceBins> {
    let mut existent = HashSet::new();
    let mut nonexistent = HashSet::new();

    for slug in page_slugs.into_iter().progress_with_style(
        ProgressStyle::with_template("Checking which pages exist {wide_bar} {pos}/{len}")
            .expect("the progress bar template should be valid"),
    ) {
        let response = ureq::get(format!("https://{}.wikidot.com/{}", site, slug)).call();
        if let Err(ureq::Error::StatusCode(404)) = response {
            nonexistent.insert(slug);
            continue;
        }
        match response?.status() {
            StatusCode::OK => existent.insert(slug),
            code => return Err(ReadError::UnexpectedStatus(code).into()),
        };
    }

    Ok(PageExistenceBins {
        existent,
        nonexistent,
    })
}

fn move_to_category(site: &str, slug: &str, category: &str) -> crate::Result<()> {
    move_page(site, slug, &format!("{}:{}", category, slug))
}

#[instrument(err)]
fn move_page(site: &str, slug: &str, new_slug: &str) -> crate::Result<()> {
    let mut normalized_new_slug = new_slug.to_owned();
    wikidot_normalize::normalize(&mut normalized_new_slug);
    if new_slug != normalized_new_slug {
        return Err(ArgError::InvalidSlug(new_slug.to_owned()).into());
    }

    todo!("move the page")
}
