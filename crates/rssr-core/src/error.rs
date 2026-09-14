use std::fmt;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Db(rusqlite::Error),
    Json(serde_json::Error),
    Xml(quick_xml::Error),
    Http {
        url: String,
        message: String,
    },
    Status {
        url: String,
        code: u16,
    },
    Parse {
        url: String,
        message: String,
    },
    Extract {
        url: String,
        message: String,
    },
    Opml(String),
    Config(String),
    /// Nothing on this machine could be asked to open a web page.
    NoOpener(String),
    /// A malformed argument. The caller made the mistake, not the network.
    Usage(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// Stable identifier for the JSON error objects the CLI emits.
    pub fn code(&self) -> &'static str {
        match self {
            Error::Io(_) => "IO_ERROR",
            Error::Db(_) => "DB_ERROR",
            Error::Json(_) => "JSON_ERROR",
            Error::Xml(_) => "XML_ERROR",
            Error::Http { .. } => "FEED_FETCH_FAILED",
            Error::Status { code: 429, .. } => "RATE_LIMITED",
            Error::Status { code: 404, .. } => "FEED_NOT_FOUND",
            Error::Status { code: 410, .. } => "FEED_GONE",
            Error::Status { .. } => "FEED_FETCH_FAILED",
            Error::Parse { .. } => "FEED_PARSE_FAILED",
            Error::Extract { .. } => "EXTRACT_FAILED",
            Error::Opml(_) => "OPML_INVALID",
            Error::Config(_) => "CONFIG_ERROR",
            Error::NoOpener(_) => "NO_OPENER",
            Error::Usage(_) => "USAGE_ERROR",
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io: {e}"),
            Error::Db(e) => write!(f, "db: {e}"),
            Error::Json(e) => write!(f, "json: {e}"),
            Error::Xml(e) => write!(f, "xml: {e}"),
            Error::Http { url, message } => write!(f, "fetch {url}: {message}"),
            Error::Status { url, code } => write!(f, "fetch {url}: http {code}"),
            Error::Parse { url, message } => write!(f, "parse {url}: {message}"),
            Error::Extract { url, message } => write!(f, "extract {url}: {message}"),
            Error::Opml(e) => write!(f, "opml: {e}"),
            Error::Config(e) => write!(f, "config: {e}"),
            Error::NoOpener(e) => write!(f, "nothing to open a page with: {e}"),
            Error::Usage(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Db(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Json(e)
    }
}

impl From<quick_xml::Error> for Error {
    fn from(e: quick_xml::Error) -> Self {
        Error::Xml(e)
    }
}
