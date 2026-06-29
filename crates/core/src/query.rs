/// A user query, optionally routed to a single extension via a keyword.
#[derive(Clone, Debug, Default)]
pub struct Query {
    /// The full, untrimmed text the user typed.
    pub raw: String,
    /// The meaningful search text. When a keyword matched, this is the text
    /// after the keyword; otherwise it is the trimmed raw input.
    pub text: String,
    /// The activation keyword, if the query was routed to a specific extension.
    pub keyword: Option<String>,
}

impl Query {
    /// A plain, unrouted query.
    pub fn new(raw: impl Into<String>) -> Self {
        let raw = raw.into();
        let text = raw.trim().to_string();
        Self {
            raw,
            text,
            keyword: None,
        }
    }

    /// A query routed to an extension by `keyword`, carrying the remaining `rest`.
    pub fn with_keyword(
        raw: impl Into<String>,
        keyword: impl Into<String>,
        rest: impl Into<String>,
    ) -> Self {
        Self {
            raw: raw.into(),
            text: rest.into(),
            keyword: Some(keyword.into()),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}
