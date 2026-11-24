use std::{
    collections::HashSet,
    fmt::Debug,
    fs::{self, File},
    io::{BufRead, BufReader, IsTerminal},
    path::{Path, PathBuf},
    time::Duration,
};

use indicatif::{ProgressIterator, ProgressStyle};
use serde::{de::DeserializeOwned, Deserialize};
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

#[derive(Debug)]
pub struct EditOptions {
    page_list_file_path: PathBuf,
    create: bool,
    forceful: bool,
    comment: Option<String>,
}

impl EditOptions {
    pub fn new(
        page_list_file_path: PathBuf,
        create: bool,
        forceful: bool,
        comment: Option<String>,
    ) -> Self {
        Self {
            page_list_file_path,
            create,
            forceful,
            comment,
        }
    }
}

#[derive(Debug)]
pub struct ReplaceOptions {
    content_file_path: PathBuf,
    discard_history: bool,
}

impl ReplaceOptions {
    pub fn new(content_file_path: PathBuf, discard_history: bool) -> Self {
        Self {
            content_file_path,
            discard_history,
        }
    }
}

#[instrument(err, skip(agent, edit_options, replace_options))]
pub fn replace(
    agent: &Agent,
    site: &str,
    bot_username: &str,
    mut edit_options: EditOptions,
    replace_options: ReplaceOptions,
    yes: bool,
) -> crate::Result<()> {
    let page_slugs = slugs_from_file(&edit_options.page_list_file_path)?;
    let contents = fs::read_to_string(replace_options.content_file_path)?;

    let PageExistenceBins {
        existent,
        nonexistent,
    } = check_existence(agent, site, &page_slugs)?;

    let info_msg = if edit_options.create && !nonexistent.is_empty() {
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
        if edit_options.comment.is_none() {
            let mut stdin = std::io::stdin().lock();
            let mut input = String::new();
            if stdin.is_terminal() {
                eprint!("No comment was provided. Provide one now (or press Enter to provide an empty comment): ");
                stdin.read_line(&mut input)?;
            }
            edit_options.comment = Some(input.trim().to_owned());
        }

        let mut prompt_msg = info_msg.clone();
        if let Some(first) = prompt_msg.get_mut(..1) {
            first.make_ascii_uppercase();
        }

        if !edit_options.create && !nonexistent.is_empty() {
            prompt_msg += ". Run this with the `--create` flag if you want to create them";
        }
        prompt(prompt_msg)?;
    }

    if edit_options.comment.as_deref().is_none_or(str::is_empty) {
        event!(Level::WARN, "running with no edition comment");
    }
    let comment = edit_options.comment.unwrap_or_default();

    event!(Level::INFO, "{info_msg}");

    for existent_page in existent {
        if replace_options.discard_history {
            move_to_category(agent, site, &existent_page, "unlisted")?;
        }
        set_contents(
            agent,
            site,
            bot_username,
            &existent_page,
            &contents,
            &comment,
            edit_options.forceful,
        )?;
    }
    if edit_options.create {
        for SlugParseResult {
            normalized,
            encountered,
        } in nonexistent
        {
            set_contents(
                agent,
                site,
                bot_username,
                &PageRef {
                    slug: normalized,
                    title: encountered,
                    details: None,
                },
                &contents,
                &comment,
                edit_options.forceful,
            )?;
        }
    }

    Ok(())
}

#[derive(PartialEq, Eq, Hash, Clone)]
struct SlugParseResult {
    normalized: String,
    encountered: String,
}

#[instrument]
fn slugs_from_file(page_list_file_path: &PathBuf) -> crate::Result<Vec<SlugParseResult>> {
    let page_list_file = File::open(page_list_file_path)?;
    let page_list_file_reader = BufReader::new(page_list_file);
    let page_slugs: Vec<_> = page_list_file_reader
        .lines()
        .map_while(Result::ok)
        .filter_map(|mut line| {
            if line.is_empty() {
                return None;
            }

            let line_in_file = line.clone();
            wikidot_normalize::normalize(&mut line);
            Some(SlugParseResult {
                normalized: line,
                encountered: line_in_file,
            })
        })
        .collect();
    event!(Level::INFO, "{} pages mentioned", page_slugs.len());
    Ok(page_slugs)
}

struct PageExistenceBins {
    existent: HashSet<PageRef>,
    nonexistent: HashSet<SlugParseResult>,
}

#[derive(Hash, PartialEq, Eq, Debug)]
struct PageRef {
    slug: String,
    title: String,
    details: Option<PageDetails>,
}

#[derive(Hash, PartialEq, Eq, Debug)]
struct PageDetails {
    id: u64,
    tags: Vec<String>,
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
    #[error("a page was found, but not its title")]
    NoPageTitle,
    #[error("page exists with a different title than what was mentioned")]
    DifferentTitle,
}

#[instrument(err, skip(agent, page_slugs))]
fn check_existence(
    agent: &Agent,
    site: &str,
    page_slugs: &[SlugParseResult],
) -> crate::Result<PageExistenceBins> {
    let mut existent = HashSet::new();
    let mut nonexistent = HashSet::new();

    for slug_parse_result in page_slugs.iter().progress_with_style(
        ProgressStyle::with_template("Checking which pages exist {wide_bar} {pos}/{len}")
            .expect("the progress bar template should be valid"),
    ) {
        let SlugParseResult {
            normalized,
            encountered,
        } = slug_parse_result;
        let response = agent
            .get(format!("https://{}.wikidot.com/{}", site, normalized))
            .call();
        if let Err(ureq::Error::StatusCode(404)) = response {
            nonexistent.insert(slug_parse_result.clone());
            continue;
        }
        let response = response?;
        match response.status() {
            StatusCode::OK => {
                let html = &response.into_body().read_to_string()?;
                let title = find_page_title_in_html(normalized, html)?;

                if normalized != encountered && title != *encountered {
                    return Err(ReadError {
                        slug: normalized.clone(),
                        kind: ReadErrorKind::DifferentTitle,
                    }
                    .into());
                }

                let id = find_page_id_in_html(normalized, html)?;
                let tags = find_page_tags_in_html(normalized, html)?;
                existent.insert(PageRef {
                    slug: normalized.clone(),
                    title,
                    details: Some(PageDetails { id, tags }),
                });
            }
            code => {
                return Err(ReadError {
                    slug: normalized.clone(),
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

fn find_page_title_in_html(slug: &str, html: &str) -> crate::Result<String> {
    let marker = "<div id=\"page-title\">\n                            ";
    let start = html.find(marker).ok_or(ReadError {
        slug: slug.to_owned(),
        kind: ReadErrorKind::NoPageId,
    })? + marker.len();
    html[start..]
        .split("\n                        <")
        .next()
        .ok_or(ReadError {
            slug: slug.to_owned(),
            kind: ReadErrorKind::NoPageTitle,
        })?
        .parse()
        .map_err(Into::into)
}

fn find_page_tags_in_html(slug: &str, html: &str) -> crate::Result<Vec<String>> {
    todo!("find page tags")
}

fn move_to_category(
    agent: &Agent,
    site: &str,
    page: &PageRef,
    category: &str,
) -> crate::Result<()> {
    move_page(agent, site, page, &format!("{}:{}", category, page.slug))
}

#[derive(Deserialize, Debug, Clone)]
struct BaseResponse {
    status: String,
}

impl WikidotResponse for BaseResponse {
    fn status(&self) -> &str {
        &self.status
    }
}

#[derive(Error, Debug)]
#[error("error while modifying page `{slug}`: {kind}")]
struct WriteError {
    slug: String,
    kind: WriteErrorKind,
}

#[derive(Error, Debug)]
enum WriteErrorKind {
    #[error("insufficient permissions")]
    NoPermission,
    #[error("Wikidot responded with something unexpected: {0:?}")]
    UnexpectedResponse(Box<dyn Debug>),
    #[error("page locked by someone else")]
    Locked,
    #[error("page locked, but Wikidot responded with something unexpected that will not let us know who locked it: {0:?}")]
    UnidentifiedLockHolder(String),
    #[error("page does not exist")]
    Nonexistent,
    #[error("page in an inconsistent state")]
    InconsistentState,
}

#[instrument(err, skip(agent))]
fn move_page(agent: &Agent, site: &str, page: &PageRef, new_slug: &str) -> crate::Result<()> {
    let mut normalized_new_slug = new_slug.to_owned();
    wikidot_normalize::normalize(&mut normalized_new_slug);
    if new_slug != normalized_new_slug {
        return Err(ArgError::InvalidSlug(new_slug.to_owned()).into());
    }

    let response: BaseResponse = module_request(
        agent,
        site,
        "Empty",
        [
            ("action", "WikiPageAction"),
            ("event", "renamePage"),
            (
                "page_id",
                &page
                    .details
                    .as_ref()
                    .ok_or(WriteErrorKind::Nonexistent)?
                    .id
                    .to_string(),
            ),
            ("new_name", new_slug),
        ],
    )?;

    if response.status == "no_permission" {
        return Err(WriteError {
            slug: page.slug.clone(),
            kind: WriteErrorKind::NoPermission,
        }
        .into());
    } else if response.status != "ok" {
        return Err(WriteError {
            slug: page.slug.clone(),
            kind: WriteErrorKind::UnexpectedResponse(Box::new(response)),
        }
        .into());
    }

    event!(Level::INFO, "moved page `{}` to `{}`", page.slug, new_slug);

    Ok(())
}

#[derive(Deserialize, Debug, Clone)]
struct LockResponse {
    status: String,
    #[serde(default)]
    locked: bool,
    lock_id: Option<u64>,
    lock_secret: Option<String>,
    page_revision_id: Option<u64>,
    body: String,
}

impl WikidotResponse for LockResponse {
    fn status(&self) -> &str {
        &self.status
    }
}

struct PageLock {
    id: u64,
    secret: String,
    revision_id: Option<u64>,
}

#[instrument(err, skip(agent, new_contents, comment))]
fn set_contents(
    agent: &Agent,
    site: &str,
    bot_username: &str,
    page: &PageRef,
    new_contents: &str,
    comment: &str,
    forceful: bool,
) -> crate::Result<()> {
    let lock = try_lock(agent, site, bot_username, &page.slug, forceful)?;

    let id_strs = match (&page.details, lock.revision_id) {
        (Some(PageDetails { id: page_id, .. }), Some(rev_id)) => {
            Some((page_id.to_string(), rev_id.to_string()))
        }
        (None, None) => None,
        _ => {
            return Err(WriteError {
                slug: page.slug.clone(),
                kind: WriteErrorKind::InconsistentState,
            }
            .into())
        }
    };
    let lock_id_str = lock.id.to_string();

    let mut params = vec![
        ("action", "WikiPageAction"),
        ("event", "savePage"),
        ("mode", "page"),
        ("wiki_page", &page.slug),
        ("lock_id", &lock_id_str),
        ("lock_secret", &lock.secret),
        ("source", new_contents),
        ("title", &page.title),
        ("comments", comment),
    ];

    if let Some((page_id_str, rev_id_str)) = &id_strs {
        params.push(("page_id", page_id_str));
        params.push(("revision_id", rev_id_str));
    }

    let response: BaseResponse = module_request(agent, site, "Empty", params)?;

    if response.status == "no_permission" {
        return Err(WriteError {
            slug: page.slug.clone(),
            kind: WriteErrorKind::NoPermission,
        }
        .into());
    } else if response.status != "ok" {
        return Err(WriteError {
            slug: page.slug.clone(),
            kind: WriteErrorKind::UnexpectedResponse(Box::new(response)),
        }
        .into());
    }

    event!(Level::INFO, "replaced contents of page `{}`", page.slug);

    Ok(())
}

#[instrument(err, skip(agent))]
fn try_lock(
    agent: &Agent,
    site: &str,
    bot_username: &str,
    slug: &str,
    forceful: bool,
) -> crate::Result<PageLock> {
    let mut lock_params = vec![("mode", "page"), ("wiki_page", slug)];
    if forceful {
        lock_params.push(("force_lock", "yes"))
    }
    let lock_response: LockResponse =
        module_request(agent, site, "edit/PageEditModule", lock_params)?;

    let unexpected_response_err = WriteError {
        slug: slug.to_owned(),
        kind: WriteErrorKind::UnexpectedResponse(Box::new(lock_response.clone())),
    };

    if lock_response.status == "no_permission" {
        return Err(WriteError {
            slug: slug.to_owned(),
            kind: WriteErrorKind::NoPermission,
        }
        .into());
    } else if lock_response.status != "ok" {
        return Err(unexpected_response_err.into());
    } else if lock_response.locked {
        let holder_username = find_user_holding_lock(slug, &lock_response.body)?;
        if holder_username == bot_username {
            return try_lock(agent, site, bot_username, slug, true);
        }

        return Err(WriteError {
            slug: slug.to_owned(),
            kind: WriteErrorKind::Locked,
        }
        .into());
    }

    match (lock_response.lock_id, lock_response.lock_secret) {
        (Some(id), Some(secret)) => Ok(PageLock {
            id,
            secret,
            revision_id: lock_response.page_revision_id,
        }),
        _ => Err(unexpected_response_err.into()),
    }
}

trait WikidotResponse: DeserializeOwned {
    fn status(&self) -> &str;
}

#[derive(Error, Debug)]
#[error("Wikidot™")]
struct WikidotError;

#[instrument(err, skip(agent, params))]
fn module_request<I, K, V, R>(
    agent: &Agent,
    site: &str,
    module_name: &str,
    params: I,
) -> crate::Result<R>
where
    I: IntoIterator<Item = (K, V)> + Debug,
    K: AsRef<str> + Debug,
    V: AsRef<str> + Debug,
    R: WikidotResponse,
{
    let params: Vec<_> = params.into_iter().collect();
    let mut attempts = 0;

    loop {
        let response: R = agent
            .post(format!(
                "https://{}.wikidot.com/ajax-module-connector.php",
                site
            ))
            .send_form(
                [
                    ("moduleName", module_name),
                    ("wikidot_token7", &crate::get_csrf_token(agent)?),
                ]
                .into_iter()
                .chain(params.iter().map(|(k, v)| (k.as_ref(), v.as_ref()))),
            )?
            .into_body()
            .read_json()?;
        attempts += 1;

        if response.status() == "try_again" {
            event!(Level::WARN, "Wikidot sent status \"try_again\"");
            if attempts < 5 {
                std::thread::sleep(Duration::from_secs(1));
            } else {
                return Err(WikidotError.into());
            }
        } else {
            return Ok(response);
        }
    }
}

#[instrument(err, skip(html))]
fn find_user_holding_lock(slug: &str, html: &str) -> crate::Result<String> {
    let error = WriteError {
        slug: slug.to_owned(),
        kind: WriteErrorKind::UnidentifiedLockHolder(html.to_owned()),
    };

    match html.match_indices("</a>").nth(1) {
        None => return Err(error.into()),
        Some((end, _)) => {
            let start = html[..end].rfind(">").ok_or(error)?;
            Ok(html[start + 1..end].to_owned())
        }
    }
}

pub fn tag(
    agent: &Agent,
    site: &str,
    page_list_file_path: &Path,
    diff_str: &str,
    yes: bool,
) -> crate::Result<()> {
    todo!("tag")
}

#[instrument(err, skip_all)]
fn prompt(prompt_msg: String) -> crate::Result<()> {
    eprint!("{}", prompt_msg + ".\n\nRun this as is? [y/N]");

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
    Ok(())
}
