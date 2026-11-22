//! The session-owned attachment store tools write images into.
//!
//! `t8ful-tools` may return an image but cannot store one: the attachment
//! directory belongs to a journal, which lives here. This adapter is the only
//! bridge, and it delegates to [`crate::images`] so a tool-returned image and a
//! clipboard attachment are validated and written by the same code.

use std::sync::{Arc, Mutex};

use t8ful_core::content::ImageInput;
use t8ful_core::tool::ToolError;
use t8ful_tools::AttachmentStore;

use crate::config::EnvSource;
use crate::images;
use crate::session::SessionId;
use crate::state::StatePaths;

/// Stores tool-returned images in the live session's attachment directory.
#[derive(Debug)]
pub struct SessionAttachments {
    paths: StatePaths,
    /// The current session id, or `None` before a journal exists.
    ///
    /// A tool is handed its store when the registry is built, and `/new` can
    /// replace the session later in the same process, so the id is read at store
    /// time rather than captured.
    session: Arc<Mutex<Option<SessionId>>>,
}

impl SessionAttachments {
    pub fn new(paths: StatePaths) -> Self {
        Self {
            paths,
            session: Arc::new(Mutex::new(None)),
        }
    }

    /// A store with state paths but no session yet.
    ///
    /// Used where a session is built without one, such as a test or a one-shot
    /// run: reading an image then reports that the feature needs a session
    /// instead of writing into an unrelated directory.
    pub fn unbound() -> Self {
        Self::new(StatePaths::discover(&EnvSource::Process))
    }

    /// Point the store at the session that is live now.
    pub fn set_session(&self, session: Option<SessionId>) {
        if let Ok(mut current) = self.session.lock() {
            *current = session;
        }
    }
}

impl AttachmentStore for SessionAttachments {
    fn store(&self, bytes: &[u8], max_encoded_bytes: usize) -> Result<ImageInput, ToolError> {
        let session = {
            let current = self
                .session
                .lock()
                .map_err(|_| ToolError::Failed("the attachment store is poisoned".to_owned()))?;

            current.clone()
        };
        let Some(session) = session else {
            return Err(ToolError::Failed(
                "image attachments require a saved session".to_owned(),
            ));
        };

        images::attach_bytes(&self.paths, &session, bytes, None, max_encoded_bytes)
            .map_err(ToolError::Failed)
    }
}

#[cfg(test)]
mod tests {
    use t8ful_tools::AttachmentStore;

    use super::SessionAttachments;
    use crate::session::SessionId;
    use crate::state::StatePaths;

    fn paths(dir: &std::path::Path) -> StatePaths {
        StatePaths {
            root: dir.join("state"),
            sessions: dir.join("state/sessions"),
            usage: dir.join("state/usage"),
        }
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();

        bytes.extend_from_slice(&13_u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        bytes.extend_from_slice(b"IEND");

        bytes
    }

    #[test]
    fn a_stored_image_lands_in_the_session_attachment_directory() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = SessionAttachments::new(paths(dir.path()));

        store.set_session(Some(SessionId::parse("session-1").unwrap()));

        let bytes = png(3, 2);
        let image = store.store(&bytes, usize::MAX).expect("stored");

        assert_eq!(image.media_type, "image/png");
        assert_eq!((image.width, image.height), (3, 2));
        assert_eq!(std::fs::read(&image.path).unwrap(), bytes);
        assert!(
            image
                .path
                .starts_with(dir.path().join("state/sessions/session-1/attachments")),
            "{}",
            image.path.display()
        );
    }

    #[test]
    fn without_a_session_an_image_is_refused_instead_of_written() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = SessionAttachments::new(paths(dir.path()));
        let error = store.store(&png(1, 1), usize::MAX).expect_err("no session");

        assert!(
            error.to_string().contains("require a saved session"),
            "{error}"
        );
        assert!(!dir.path().join("state").exists(), "nothing was created");
    }

    #[test]
    fn the_encoded_limit_is_enforced_by_the_shared_validation_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = SessionAttachments::new(paths(dir.path()));

        store.set_session(Some(SessionId::parse("session-1").unwrap()));

        let error = store.store(&png(4, 4), 4).expect_err("over the limit");

        assert!(
            error.to_string().contains("base64-encoded image"),
            "{error}"
        );
    }
}
