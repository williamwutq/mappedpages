use std::fmt;

#[derive(Debug)]
pub enum MappedPageError {
    // variants will be added during implementation
}

impl fmt::Display for MappedPageError {
    fn fmt(&self, _f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {}
    }
}

impl std::error::Error for MappedPageError {}
