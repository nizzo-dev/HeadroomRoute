use anyhow::{Context, Result, anyhow};
use std::{
    collections::HashMap,
    io::{Read, Write},
    net::TcpStream,
};

pub(super) const MAX_HEADER: usize = 64 * 1024;
pub(super) const MAX_BODY: usize = 32 * 1024 * 1024;

pub(super) struct Incoming {
    pub(super) method: String,
    pub(super) target: String,
    pub(super) headers: HashMap<String, String>,
    pub(super) body: Vec<u8>,
}

pub(super) fn read_request(stream: &mut TcpStream) -> Result<Incoming> {
    let mut data = Vec::with_capacity(8192);
    let mut buffer = [0u8; 8192];
    let header_end;
    loop {
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            return Err(anyhow!("连接在请求头完成前关闭"));
        }
        data.extend_from_slice(&buffer[..count]);
        if let Some(index) = data.windows(4).position(|part| part == b"\r\n\r\n") {
            header_end = index + 4;
            break;
        }
        if data.len() > MAX_HEADER {
            return Err(anyhow!("请求头过大"));
        }
    }
    let mut parsed_headers = [httparse::EMPTY_HEADER; 96];
    let mut request = httparse::Request::new(&mut parsed_headers);
    request
        .parse(&data[..header_end])
        .context("HTTP 请求头无法解析")?;
    let method = request
        .method
        .ok_or_else(|| anyhow!("缺少 HTTP 方法"))?
        .to_owned();
    let target = request
        .path
        .ok_or_else(|| anyhow!("缺少请求路径"))?
        .to_owned();
    let headers: HashMap<String, String> = request
        .headers
        .iter()
        .map(|h| {
            (
                h.name.to_ascii_lowercase(),
                String::from_utf8_lossy(h.value).into_owned(),
            )
        })
        .collect();
    let length = headers
        .get("content-length")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    if length > MAX_BODY {
        return Err(anyhow!("请求体超过 32 MiB"));
    }
    let mut body = data[header_end..].to_vec();
    if headers
        .get("transfer-encoding")
        .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"))
    {
        loop {
            if let Some(decoded) = decode_chunked(&body)? {
                body = decoded;
                break;
            }
            let count = stream.read(&mut buffer)?;
            if count == 0 {
                return Err(anyhow!("分块请求体未完成"));
            }
            body.extend_from_slice(&buffer[..count]);
            if body.len() > MAX_BODY + MAX_HEADER {
                return Err(anyhow!("请求体超过 32 MiB"));
            }
        }
    } else {
        while body.len() < length {
            let count = stream.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            body.extend_from_slice(&buffer[..count]);
        }
        body.truncate(length);
    }
    Ok(Incoming {
        method,
        target,
        headers,
        body,
    })
}

pub(super) fn write_json(
    stream: &mut TcpStream,
    status: u16,
    value: serde_json::Value,
) -> Result<()> {
    let body = serde_json::to_vec(&value)?;
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        409 => "Conflict",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Response",
    };
    stream.write_all(format!("HTTP/1.1 {status} {reason}\r\nContent-Type: application/json; charset=utf-8\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes())?;
    stream.write_all(&body)?;
    Ok(())
}

pub(super) fn decode_chunked(raw: &[u8]) -> Result<Option<Vec<u8>>> {
    let mut offset = 0usize;
    let mut decoded = Vec::new();
    loop {
        let Some(line_end) = raw[offset..]
            .windows(2)
            .position(|part| part == b"\r\n")
            .map(|index| offset + index)
        else {
            return Ok(None);
        };
        let size_text = std::str::from_utf8(&raw[offset..line_end])?
            .split(';')
            .next()
            .unwrap_or_default();
        let size = usize::from_str_radix(size_text.trim(), 16).context("无效分块长度")?;
        offset = line_end + 2;
        if size == 0 {
            if raw.len() < offset + 2 {
                return Ok(None);
            }
            return Ok(Some(decoded));
        }
        if decoded.len() + size > MAX_BODY {
            return Err(anyhow!("请求体超过 32 MiB"));
        }
        if raw.len() < offset + size + 2 {
            return Ok(None);
        }
        decoded.extend_from_slice(&raw[offset..offset + size]);
        offset += size;
        if &raw[offset..offset + 2] != b"\r\n" {
            return Err(anyhow!("无效分块边界"));
        }
        offset += 2;
    }
}
