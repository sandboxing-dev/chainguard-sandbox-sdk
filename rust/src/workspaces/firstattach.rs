//! Completing a new workspace session's first attach.

use std::io::Write as _;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use super::Client;
use super::pb::{AttachRequest, AttachResponse, attach_request, attach_response};
use crate::{Error, Result};

/// Bounds [`Client::complete_first_attach`].
pub const FIRST_ATTACH_TIMEOUT: Duration = Duration::from_secs(120);

/// An empty tar archive: the 1 KiB end-of-archive marker.
const EMPTY_TAR: [u8; 1024] = [0; 1024];

fn ended() -> Error {
    Error::other("workspaces: the attach ended before the workspace was ready")
}

impl Client {
    /// Completes a new workspace session's first attach, which the plane requires before it
    /// allows Exec (until then Exec fails with FailedPrecondition "attach with a shell first to
    /// upload the workspace").
    ///
    /// It opens one AttachSession stream with a shell, and if the plane reports
    /// workspace_pending, uploads an empty tree: a single FULL_TAR chunk holding an empty tar
    /// archive (gzip'd when the plane offers GZIP). It waits for WorkspaceReady and then detaches
    /// by canceling the stream. A session that isn't pending is left as it is. It gives up after
    /// [`FIRST_ATTACH_TIMEOUT`] (an [`Error::Timeout`] with op "start").
    pub async fn complete_first_attach(&self, name: &str) -> Result<()> {
        match tokio::time::timeout(FIRST_ATTACH_TIMEOUT, self.first_attach(name)).await {
            Ok(r) => r,
            Err(_) => Err(Error::Timeout {
                op: "start".into(),
                detail: "the first attach did not complete".into(),
                result: None,
            }),
        }
    }

    async fn first_attach(&self, name: &str) -> Result<()> {
        let (tx, rx) = mpsc::channel::<AttachRequest>(4);
        let open = AttachRequest {
            input: Some(attach_request::Input::Open(attach_request::Open {
                name: name.into(),
                // An empty Resume is sent, as chainctl does.
                resume: Some(attach_request::Resume::default()),
            })),
        };
        tx.send(open).await.map_err(|_| ended())?;
        let r = self.request(ReceiverStream::new(rx)).await?;
        // Dropping the response stream and the sender is the detach.
        let mut stream = self.check(self.raw().attach_session(r).await)?.into_inner();
        loop {
            let resp: AttachResponse = match stream.message().await {
                Ok(Some(m)) => m,
                Ok(None) => return Err(ended()),
                Err(s) => return Err(self.status(s)),
            };
            match resp.output {
                Some(attach_response::Output::Opened(o)) => {
                    if !o.workspace_pending {
                        return Ok(());
                    }
                    let chunk = empty_workspace_chunk(&o.workspace_encodings)?;
                    let req = AttachRequest {
                        input: Some(attach_request::Input::WorkspaceChunk(chunk)),
                    };
                    if tx.send(req).await.is_err() {
                        // The stream closed: its status arrives on the next reads.
                        loop {
                            match stream.message().await {
                                Ok(Some(_)) => continue,
                                Ok(None) => return Err(ended()),
                                Err(s) => return Err(self.status(s)),
                            }
                        }
                    }
                }
                Some(attach_response::Output::WorkspaceReady(_)) => return Ok(()),
                Some(attach_response::Output::Detached(_)) => return Err(Error::Detached),
                // Anything else the plane sends on a shell attach (terminal output, for one)
                // isn't in this SDK's protocol subset and arrives with no output set: skipped.
                None => {}
            }
        }
    }
}

fn empty_workspace_chunk(encodings: &[i32]) -> Result<attach_request::WorkspaceChunk> {
    use attach_request::workspace_chunk::{Encoding, Format};
    let mut chunk = attach_request::WorkspaceChunk {
        data: EMPTY_TAR.to_vec(),
        last: true,
        format: Format::FullTar as i32,
        encoding: Encoding::Unspecified as i32,
    };
    if encodings.contains(&(Encoding::Gzip as i32)) {
        let mut zw = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        zw.write_all(&EMPTY_TAR).map_err(|e| Error::io("gzip", e))?;
        chunk.data = zw.finish().map_err(|e| Error::io("gzip", e))?;
        chunk.encoding = Encoding::Gzip as i32;
    }
    Ok(chunk)
}
