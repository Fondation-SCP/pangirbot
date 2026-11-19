use std::{
    collections::HashSet,
    fs::File,
    io::{BufRead, BufReader},
    path::Path,
};

use thiserror::Error;
use tracing::{event, instrument, Level};

#[derive(Error, Debug)]
enum ArgError {
    #[error("not a valid slug")]
    InvalidSlug(String),
}

#[instrument(err)]
pub fn replace(
    site: &str,
    page_list_file_path: &Path,
    content_file_path: &Path,
) -> crate::Result<()> {
    let page_slugs = slugs_from_file(page_list_file_path)?;

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
