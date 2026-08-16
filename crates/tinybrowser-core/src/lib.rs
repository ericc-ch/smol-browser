pub mod context;
pub mod lifecycle;
pub mod page;
pub mod profiles;

pub use context::BrowserContext;
pub use lifecycle::{LifecycleState, WaitUntil};
pub use tinybrowser_js::HTML_TO_MARKDOWN_JS;
pub use page::{NetworkEvent, Page, PageError};
// Re-exported so the embeddable `tinybrowser-lib` crate (which depends on tinybrowser-core,
// not tinybrowser-js) can surface the interception channel types.
pub use tinybrowser_js::ops::{InterceptResolution, InterceptedRequest};
