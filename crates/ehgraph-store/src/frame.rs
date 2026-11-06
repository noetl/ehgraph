use ehgraph_core::{EhError, FrameBody};

const MAGIC: &[u8; 4] = b"EHGR";
const VERSION: u8 = 1;
const HEADER: usize = 13;

/// `magic || version || len || crc32 || body`, the EHDB L0 frame shape with ehgraph's magic.
pub fn encode(body: &FrameBody) -> Result<Vec<u8>, EhError> {
    let json = serde_json::to_vec(body)?;
    let mut out = Vec::with_capacity(HEADER + json.len());
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.extend_from_slice(&(json.len() as u32).to_le_bytes());
    let crc = crc32fast::hash(&json);
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&json);
    Ok(out)
}

#[derive(Debug)]
pub struct Scan {
    pub frames: Vec<FrameBody>,
    /// Bytes that form complete, valid frames.
    pub good_len: usize,
    /// A torn tail (crash mid-append) was ignored.
    pub torn: bool,
}

/// Decode a part. A torn final frame is reported rather than applied.
/// A checksum failure before EOF is corruption.
pub fn decode_part(bytes: &[u8]) -> Result<Scan, EhError> {
    let mut frames = Vec::new();
    let mut offset = 0usize;
    while offset < bytes.len() {
        let rest = &bytes[offset..];
        if rest.len() < HEADER {
            return Ok(Scan {
                frames,
                good_len: offset,
                torn: true,
            });
        }
        if &rest[0..4] != MAGIC {
            return Ok(Scan {
                frames,
                good_len: offset,
                torn: true,
            });
        }
        if rest[4] != VERSION {
            return Err(EhError::unavailable(format!(
                "unsupported change-log frame version {}",
                rest[4]
            )));
        }
        let len = u32::from_le_bytes(rest[5..9].try_into().expect("4 bytes")) as usize;
        let crc = u32::from_le_bytes(rest[9..13].try_into().expect("4 bytes"));
        let total = HEADER + len;
        if rest.len() < total {
            return Ok(Scan {
                frames,
                good_len: offset,
                torn: true,
            });
        }
        let body = &rest[HEADER..total];
        let actual = crc32fast::hash(body);
        if actual != crc {
            if offset + total == bytes.len() {
                return Ok(Scan {
                    frames,
                    good_len: offset,
                    torn: true,
                });
            }
            return Err(EhError::unavailable(
                "change-log checksum mismatch before end of part",
            ));
        }
        let parsed: FrameBody = serde_json::from_slice(body).map_err(|err| {
            EhError::unavailable(format!("change-log frame json is corrupt: {err}"))
        })?;
        frames.push(parsed);
        offset += total;
    }
    Ok(Scan {
        frames,
        good_len: offset,
        torn: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ehgraph_core::{FrameBody, IdempotencyNote};

    #[test]
    fn roundtrip_and_torn_tail() {
        let frame = FrameBody::Idempotency(IdempotencyNote {
            namespace: "lab".into(),
            key: "k".into(),
            body_hash: "ab".into(),
            graph_version: 1,
        });
        let bytes = encode(&frame).unwrap();
        let scan = decode_part(&bytes).unwrap();
        assert_eq!(scan.frames.len(), 1);
        assert!(!scan.torn);

        let mut torn = bytes.clone();
        torn.extend_from_slice(&[1, 2, 3, 4]);
        let scan = decode_part(&torn).unwrap();
        assert!(scan.torn);
        assert_eq!(scan.frames.len(), 1);
        assert_eq!(scan.good_len, bytes.len());
    }
}
