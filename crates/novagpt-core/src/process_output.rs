//! Drain child pipes without unbounded allocations or producer deadlocks.
use tokio::io::{AsyncRead, AsyncReadExt};

pub(crate) async fn read_bounded<R: AsyncRead + Unpin>(
    mut pipe: R,
    limit: usize,
) -> std::io::Result<(Vec<u8>, bool)> {
    let mut content = Vec::new();
    let mut overflow = false;
    let mut buffer = [0u8; 4096];
    loop {
        let count = pipe.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        let remaining = limit.saturating_sub(content.len());
        let kept = count.min(remaining);
        content.extend_from_slice(&buffer[..kept]);
        if count > kept {
            overflow = true;
        }
    }
    Ok((content, overflow))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn bounded_pipe_keeps_prefix_and_drains_remainder() {
        let pipe: &[u8] = b"0123456789";
        let (contents, truncated) = read_bounded(pipe, 4).await.expect("read");
        assert_eq!(contents, b"0123");
        assert!(truncated);
    }
}
