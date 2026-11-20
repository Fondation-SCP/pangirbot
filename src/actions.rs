use std::{
    collections::HashSet,
    fs::File,
    io::{BufRead, BufReader, IsTerminal},
    path::Path,
};

use indicatif::{ProgressIterator, ProgressStyle};
use thiserror::Error;
use tracing::{event, instrument, Level};
use ureq::{http::StatusCode, Agent};

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
    agent: &Agent,
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
    } = check_existence(agent, site, page_slugs)?;

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
            move_to_category(agent, site, &existent_page, "unlisted")?;
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
    existent: HashSet<PageRef>,
    nonexistent: HashSet<String>,
}

#[derive(Hash, PartialEq, Eq, Debug)]
struct PageRef {
    slug: String,
    id: u64,
}

#[derive(Error, Debug)]
#[error("error while reading page `{slug}`: {kind}")]
struct ReadError {
    slug: String,
    kind: ReadErrorKind,
}

#[derive(Error, Debug)]
enum ReadErrorKind {
    #[error("Wikidot responded with an unexpected status")]
    UnexpectedStatus(StatusCode),
    #[error("a page was found, but not its numeric ID")]
    NoPageId,
}

#[instrument(err, skip(page_slugs))]
fn check_existence(
    agent: &Agent,
    site: &str,
    page_slugs: HashSet<String>,
) -> crate::Result<PageExistenceBins> {
    let mut existent = HashSet::new();
    let mut nonexistent = HashSet::new();

    for slug in page_slugs.into_iter().progress_with_style(
        ProgressStyle::with_template("Checking which pages exist {wide_bar} {pos}/{len}")
            .expect("the progress bar template should be valid"),
    ) {
        let response = agent
            .get(format!("https://{}.wikidot.com/{}", site, slug))
            .call();
        if let Err(ureq::Error::StatusCode(404)) = response {
            nonexistent.insert(slug);
            continue;
        }
        let response = response?;
        match response.status() {
            StatusCode::OK => {
                let id = find_page_id_in_html(&slug, &response.into_body().read_to_string()?)?;
                existent.insert(PageRef { slug, id });
            }
            code => {
                return Err(ReadError {
                    slug,
                    kind: ReadErrorKind::UnexpectedStatus(code),
                }
                .into())
            }
        };
    }

    Ok(PageExistenceBins {
        existent,
        nonexistent,
    })
}

fn find_page_id_in_html(slug: &str, html: &str) -> crate::Result<u64> {
    let marker = "WIKIREQUEST.info.pageId = ";
    let start = html.find(marker).ok_or(ReadError {
        slug: slug.to_owned(),
        kind: ReadErrorKind::NoPageId,
    })? + marker.len();
    html[start..]
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .ok_or(ReadError {
            slug: slug.to_owned(),
            kind: ReadErrorKind::NoPageId,
        })?
        .parse()
        .map_err(Into::into)
}

fn move_to_category(
    agent: &Agent,
    site: &str,
    page: &PageRef,
    category: &str,
) -> crate::Result<()> {
    move_page(agent, site, page, &format!("{}:{}", category, page.slug))
}

#[instrument(err)]
fn move_page(agent: &Agent, site: &str, page: &PageRef, new_slug: &str) -> crate::Result<()> {
    let mut normalized_new_slug = new_slug.to_owned();
    wikidot_normalize::normalize(&mut normalized_new_slug);
    if new_slug != normalized_new_slug {
        return Err(ArgError::InvalidSlug(new_slug.to_owned()).into());
    }

    agent
        .post(format!(
            "https://{}.wikidot.com/ajax-module-connector.php",
            site
        ))
        .send_form([
            ("moduleName", "Empty"),
            ("action", "WikiPageAction"),
            ("event", "renamePage"),
            ("page_id", &page.id.to_string()),
            ("new_name", new_slug),
            ("wikidot_token7", &crate::get_csrf_token(agent)?),
        ])?;
    Ok(())
}
