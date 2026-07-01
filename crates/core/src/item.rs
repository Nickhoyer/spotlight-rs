use std::path::PathBuf;
use std::sync::Arc;

/// How to draw the leading icon for a result.
#[derive(Clone, Debug)]
pub enum Icon {
    /// A file/bundle whose system icon should be shown (e.g. an `.app`).
    File(PathBuf),
    /// An image file (e.g. a logo PNG) rendered directly, as opposed to
    /// [`Icon::File`] which shows the OS-provided file icon.
    Image(PathBuf),
    /// A glyph or emoji to render directly as text.
    Glyph(String),
    /// Raw RGBA pixels with their dimensions.
    Rgba {
        width: u32,
        height: u32,
        data: Arc<Vec<u8>>,
    },
}

/// What happens when the user activates (presses Enter on) a result.
#[derive(Clone, Debug)]
pub enum Action {
    /// Open a path with the OS default handler (used to launch apps).
    Open(PathBuf),
    /// Open a URL in the default browser.
    OpenUrl(String),
    /// Copy text to the clipboard.
    Copy(String),
    /// Navigate to a full-screen extension panel by its id (e.g. open the
    /// Clipboard History panel from a search result).
    OpenPanel(String),
    /// An extension-specific action, dispatched back to its owning extension's
    /// [`Extension::run`] with this id.
    Custom(String),
    /// No-op (informational results).
    None,
}

/// A single row in the results list.
#[derive(Clone, Debug)]
pub struct ResultItem {
    /// Stable identifier, unique within a query (used for list keys).
    pub id: String,
    pub title: String,
    pub subtitle: Option<String>,
    pub icon: Option<Icon>,
    pub action: Action,
    /// Ranking score; higher sorts first.
    pub score: i32,
    /// Id of the [`Extension`] that produced this item.
    pub source: String,
}
