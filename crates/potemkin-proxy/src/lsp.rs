//! LSP wire-format helpers: reading `Content-Length`-framed messages and
//! splitting a framed message into (headers, JSON payload).

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncReadExt};

/// Read one complete `Content-Length`-framed LSP message from `reader`.
///
/// Parses the full header block (any number of `\r\n`-terminated headers, e.g. an
/// optional `Content-Type`, followed by a blank line) rather than assuming a
/// single header. Returns `Ok(None)` on clean EOF. The returned string is
/// re-framed so it can be forwarded verbatim when no transformation is needed.
pub async fn read_message<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
    line: &mut String,
) -> Result<Option<String>> {
    let mut content_length: Option<usize> = None;

    // Read headers until the blank line that terminates the header block.
    loop {
        line.clear();
        match reader.read_line(line).await {
            Ok(0) => return Ok(None), // EOF (between messages, or mid-headers)
            Ok(_) => {}
            Err(e) => return Err(anyhow::anyhow!("failed to read LSP header: {e}")),
        }

        let header = line.trim_end_matches(['\r', '\n']);
        if header.is_empty() {
            break; // end of header block
        }

        if let Some(rest) = header
            .split_once(':')
            .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .map(|(_, v)| v.trim())
        {
            content_length = Some(
                rest.parse()
                    .map_err(|e| anyhow::anyhow!("invalid Content-Length '{rest}': {e}"))?,
            );
        }
        // Any other header (e.g. Content-Type) is accepted and ignored.
    }

    let content_length =
        content_length.ok_or_else(|| anyhow::anyhow!("LSP message missing Content-Length"))?;

    let mut json = vec![0u8; content_length];
    reader.read_exact(&mut json).await?;
    let json = String::from_utf8(json)?;

    Ok(Some(frame(&json)))
}

/// Wrap a JSON payload in an LSP `Content-Length` frame.
pub fn frame(json: &str) -> String {
    format!("Content-Length: {}\r\n\r\n{}", json.len(), json)
}

/// Extract the JSON payload from a framed LSP message.
pub fn payload(message: &str) -> Option<&str> {
    message.find("\r\n\r\n").map(|pos| &message[pos + 4..])
}
