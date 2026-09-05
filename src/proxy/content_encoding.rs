use axum::http::HeaderMap;
use std::io::{self, Cursor, Read};

pub(crate) const MAX_REQUEST_BODY_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub(crate) enum DecompressError {
    #[error("decompression failed: {0}")]
    Io(#[from] io::Error),
    #[error("decompressed body exceeds {limit} bytes")]
    TooLarge { limit: usize },
}

fn split_codings(value: &str) -> Vec<&str> {
    value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty() && !value.eq_ignore_ascii_case("identity"))
        .collect()
}

pub(crate) fn get_content_encoding(headers: &HeaderMap) -> Option<String> {
    let values = headers
        .get_all("content-encoding")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(split_codings)
        .collect::<Vec<_>>();
    (!values.is_empty()).then(|| values.join(", "))
}

pub(crate) fn is_supported_content_encoding(value: &str) -> bool {
    let codings = split_codings(value);
    !codings.is_empty()
        && codings.iter().all(|coding| {
            matches!(
                coding.to_ascii_lowercase().as_str(),
                "gzip" | "x-gzip" | "deflate" | "br" | "zstd" | "zst"
            )
        })
}

fn read_with_limit(reader: impl Read, limit: usize) -> Result<Vec<u8>, DecompressError> {
    let mut output = Vec::new();
    reader
        .take(limit.saturating_add(1) as u64)
        .read_to_end(&mut output)?;
    if output.len() > limit {
        return Err(DecompressError::TooLarge { limit });
    }
    Ok(output)
}

fn decode_one(coding: &str, input: &[u8], limit: usize) -> Result<Vec<u8>, DecompressError> {
    match coding.to_ascii_lowercase().as_str() {
        "gzip" | "x-gzip" => {
            read_with_limit(flate2::read::GzDecoder::new(Cursor::new(input)), limit)
        }
        "deflate" => {
            let zlib_result =
                read_with_limit(flate2::read::ZlibDecoder::new(Cursor::new(input)), limit);
            match zlib_result {
                Ok(output) => Ok(output),
                Err(DecompressError::TooLarge { .. }) => zlib_result,
                Err(zlib_error) => {
                    read_with_limit(flate2::read::DeflateDecoder::new(Cursor::new(input)), limit)
                        .or(Err(zlib_error))
                }
            }
        }
        "br" => read_with_limit(brotli::Decompressor::new(Cursor::new(input), 4096), limit),
        "zstd" | "zst" => {
            read_with_limit(zstd::stream::read::Decoder::new(Cursor::new(input))?, limit)
        }
        _ => unreachable!("unsupported content encoding should be rejected before decoding"),
    }
}

pub(crate) fn decompress_body_with_limit(
    content_encoding: &str,
    body: &[u8],
    limit: usize,
) -> Result<Option<Vec<u8>>, DecompressError> {
    let codings = split_codings(content_encoding);
    if codings.is_empty() {
        return Ok(None);
    }
    if !is_supported_content_encoding(content_encoding) {
        return Ok(None);
    }

    let mut decoded = body.to_vec();
    for coding in codings.into_iter().rev() {
        decoded = decode_one(coding, &decoded, limit)?;
    }
    Ok(Some(decoded))
}

#[allow(dead_code)]
pub(crate) fn decompress_body(
    content_encoding: &str,
    body: &[u8],
) -> Result<Option<Vec<u8>>, DecompressError> {
    decompress_body_with_limit(content_encoding, body, usize::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use std::io::Write;

    #[test]
    fn round_trips_supported_encodings() {
        let payload = br#"{"hello":"world","n":42}"#;

        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gzip.write_all(payload).unwrap();
        let gzip = gzip.finish().unwrap();

        let mut deflate =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        deflate.write_all(payload).unwrap();
        let deflate = deflate.finish().unwrap();

        let mut brotli = Vec::new();
        brotli::CompressorWriter::new(&mut brotli, 4096, 5, 22)
            .write_all(payload)
            .unwrap();

        let zstd = zstd::stream::encode_all(std::io::Cursor::new(payload), 0).unwrap();

        for (encoding, compressed) in [
            ("gzip", gzip),
            ("deflate", deflate),
            ("br", brotli),
            ("zstd", zstd),
        ] {
            assert_eq!(
                decompress_body(encoding, &compressed).unwrap().unwrap(),
                payload
            );
        }
    }

    #[test]
    fn decodes_stacked_content_encoding_in_reverse_order() {
        let payload = br#"{"stacked":true}"#;
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gzip.write_all(payload).unwrap();
        let gzip = gzip.finish().unwrap();
        let stacked = zstd::stream::encode_all(std::io::Cursor::new(gzip), 0).unwrap();

        assert_eq!(
            decompress_body("gzip, zstd", &stacked).unwrap().unwrap(),
            payload
        );
    }

    #[test]
    fn rejects_unknown_or_corrupt_encoding() {
        assert!(!is_supported_content_encoding("compress"));
        assert!(is_supported_content_encoding("GZip"));
        assert!(decompress_body("compress", b"body").unwrap().is_none());
        assert!(decompress_body("zstd", b"not-zstd").is_err());
    }

    #[test]
    fn combines_repeated_headers_and_ignores_identity() {
        let mut headers = HeaderMap::new();
        headers.append("content-encoding", HeaderValue::from_static("gzip"));
        headers.append("content-encoding", HeaderValue::from_static("zstd"));
        assert_eq!(
            get_content_encoding(&headers).as_deref(),
            Some("gzip, zstd")
        );

        let mut identity = HeaderMap::new();
        identity.insert("content-encoding", HeaderValue::from_static("identity"));
        assert_eq!(get_content_encoding(&identity), None);
    }

    #[test]
    fn enforces_decompressed_size_limit() {
        let payload = vec![b'x'; 1024];
        let compressed = zstd::stream::encode_all(std::io::Cursor::new(payload), 0).unwrap();
        assert!(decompress_body_with_limit("zstd", &compressed, 32).is_err());
    }
}
