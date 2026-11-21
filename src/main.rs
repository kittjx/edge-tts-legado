//! edge-tts-legado: 把微软 Edge「大声朗读」包装成 HTTP 接口，给 Legado(阅读) 当 HTTP 朗读引擎用。
//!
//! GET/POST /tts?text=...&voice=zh-CN-XiaoxiaoNeural&rate=0&pitch=0&volume=0
//!   - text 可放在 query，也可以放在 POST 正文（纯文本）
//!   - rate / volume 单位 %，pitch 单位 Hz，均可为负数
//! GET /voices   列出可用发音人
//! GET /health   健康检查
//!
//! 环境变量：PORT（默认 3000）、HOST（默认 0.0.0.0）、TOKEN（可选，设置后需要鉴权）、CONCURRENCY（同时合成数，默认 2）

use axum::{
    body::Bytes,
    extract::{Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{any, get},
    Json, Router,
};
use msedge_tts::{tts::client::connect, tts::SpeechConfig, voice::get_voices_list};
use serde::Deserialize;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

const DEFAULT_VOICE: &str = "zh-CN-XiaoxiaoNeural";
const DEFAULT_FORMAT: &str = "audio-24khz-48kbitrate-mono-mp3";
/// 单次发给微软的文本上限（转义后的 UTF-8 字节数）。微软单条 SSML 有长度上限，
/// 按字节而不是字符数算，中文一个字是 3 字节。超过会按标点切开再拼接音频
const CHUNK_BYTES: usize = 2000;
/// 找不到句末标点时的硬上限
const CHUNK_HARD_BYTES: usize = 3000;
/// 单次合成失败后的最大尝试次数
const MAX_ATTEMPTS: u32 = 3;

#[derive(Clone)]
struct AppState {
    token: Option<Arc<str>>,
    /// 同时向微软发起的合成请求数上限，避免被限流
    limit: Arc<Semaphore>,
    /// 为 true 时每个成功请求也打印日志（环境变量 VERBOSE=1）
    verbose: bool,
}

#[derive(Deserialize, Default)]
struct Params {
    text: Option<String>,
    voice: Option<String>,
    rate: Option<i32>,
    pitch: Option<i32>,
    volume: Option<i32>,
    format: Option<String>,
    token: Option<String>,
}

fn err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, msg.into()).into_response()
}

fn authorized(state: &AppState, headers: &HeaderMap, q: &Params) -> bool {
    let Some(expected) = &state.token else {
        return true;
    };
    if q.token.as_deref() == Some(expected.as_ref()) {
        return true;
    }
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|t| t == expected.as_ref())
        .unwrap_or(false)
}

/// SSML 里的特殊字符转义，控制字符替换成空格
fn escape_ssml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c if c.is_control() && c != '\n' => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// 按句末标点把长文本切成片段，每段转义后不超过 CHUNK_BYTES 字节（返回的已是转义后的 SSML 文本）
fn split_text(text: &str) -> Vec<String> {
    const ENDS: &[char] = &['。', '！', '？', '；', '!', '?', ';', '\n', '…'];
    let mut chunks = Vec::new();
    let mut cur = String::new();
    let mut bytes = 0usize;
    let mut has_text = false;
    let mut buf = [0u8; 4];
    for c in text.chars() {
        let esc = escape_ssml(c.encode_utf8(&mut buf));
        bytes += esc.len();
        cur.push_str(&esc);
        has_text |= c.is_alphanumeric();
        let at_end = ENDS.contains(&c);
        if (bytes >= CHUNK_BYTES && at_end) || bytes >= CHUNK_HARD_BYTES {
            // 没有任何文字/数字的片段（纯标点、空白）微软会报错，直接丢掉
            let chunk = std::mem::take(&mut cur);
            if has_text {
                chunks.push(chunk);
            }
            bytes = 0;
            has_text = false;
        }
    }
    if has_text {
        chunks.push(cur);
    }
    chunks
}

/// 阻塞式合成：整个请求复用一条 WebSocket；失败后等待一小会儿再重连，最多尝试 MAX_ATTEMPTS 次
fn synthesize(chunks: Vec<String>, cfg: SpeechConfig) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut client = None;
    for chunk in &chunks {
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            if client.is_none() {
                match connect() {
                    Ok(c) => client = Some(c),
                    Err(e) => {
                        if attempt >= MAX_ATTEMPTS {
                            return Err(format!("connect failed: {e}"));
                        }
                        eprintln!("connect error (attempt {attempt}): {e}");
                        std::thread::sleep(Duration::from_millis(400 * attempt as u64));
                        continue;
                    }
                }
            }
            match client.as_mut().unwrap().synthesize(chunk, &cfg) {
                Ok(audio) => {
                    out.extend(audio.audio_bytes);
                    break;
                }
                Err(e) => {
                    // 连接已不可用，丢弃，下一轮重连
                    client = None;
                    if attempt >= MAX_ATTEMPTS {
                        return Err(format!("synthesize failed after {attempt} attempts: {e}"));
                    }
                    eprintln!("synthesize error (attempt {attempt}): {e}");
                    std::thread::sleep(Duration::from_millis(400 * attempt as u64));
                }
            }
        }
    }
    Ok(out)
}

async fn tts(
    State(state): State<AppState>,
    Query(q): Query<Params>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !authorized(&state, &headers, &q) {
        return err(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    let text = match q.text.clone() {
        Some(t) if !t.trim().is_empty() => t,
        _ => String::from_utf8_lossy(&body).into_owned(),
    };
    if text.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "missing text (query ?text= or request body)");
    }
    let chunks = split_text(&text);
    if chunks.is_empty() {
        return err(StatusCode::BAD_REQUEST, "text has nothing to read");
    }
    let cfg = SpeechConfig {
        voice_name: q.voice.clone().unwrap_or_else(|| DEFAULT_VOICE.to_owned()),
        audio_format: q.format.clone().unwrap_or_else(|| DEFAULT_FORMAT.to_owned()),
        pitch: q.pitch.unwrap_or(0).clamp(-100, 100),
        rate: q.rate.unwrap_or(0).clamp(-100, 200),
        volume: q.volume.unwrap_or(0).clamp(-100, 100),
    };
    let content_type = if cfg.audio_format.contains("mp3") {
        "audio/mpeg"
    } else if cfg.audio_format.contains("opus") {
        "audio/webm"
    } else {
        "application/octet-stream"
    };

    let preview: String = text.chars().filter(|c| !c.is_whitespace()).take(20).collect();
    let total_chars = text.chars().count();
    let n_chunks = chunks.len();
    let started = Instant::now();

    // 限制同时进行的合成数；排队时间也算在耗时里
    let _permit = state.limit.acquire().await.expect("semaphore closed");
    let result = tokio::task::spawn_blocking(move || synthesize(chunks, cfg)).await;
    let ms = started.elapsed().as_millis();

    match result {
        Ok(Ok(audio)) if !audio.is_empty() => {
            if state.verbose {
                println!("ok   {ms}ms chars={total_chars} chunks={n_chunks} bytes={} \"{preview}\"", audio.len());
            }
            ([(header::CONTENT_TYPE, content_type)], audio).into_response()
        }
        Ok(Ok(_)) => {
            eprintln!("FAIL {ms}ms chars={total_chars} empty audio \"{preview}\"");
            err(StatusCode::BAD_GATEWAY, "empty audio from upstream")
        }
        Ok(Err(e)) => {
            eprintln!("FAIL {ms}ms chars={total_chars} chunks={n_chunks} \"{preview}\": {e}");
            err(StatusCode::BAD_GATEWAY, e)
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, format!("task error: {e}")),
    }
}

async fn voices(
    State(state): State<AppState>,
    Query(q): Query<Params>,
    headers: HeaderMap,
) -> Response {
    if !authorized(&state, &headers, &q) {
        return err(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    match tokio::task::spawn_blocking(get_voices_list).await {
        Ok(Ok(list)) => Json(list).into_response(),
        Ok(Err(e)) => err(StatusCode::BAD_GATEWAY, format!("voice list failed: {e}")),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, format!("task error: {e}")),
    }
}

#[tokio::main]
async fn main() {
    let host = std::env::var("HOST").unwrap_or_else(|_| "0.0.0.0".into());
    let port = std::env::var("PORT").unwrap_or_else(|_| "3000".into());
    let token = std::env::var("TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
        .map(|t| Arc::from(t.as_str()));
    let concurrency: usize = std::env::var("CONCURRENCY")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(2);
    println!("max concurrent synth requests: {concurrency}");
    let verbose = matches!(
        std::env::var("VERBOSE").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE") | Ok("yes")
    );
    let state = AppState {
        verbose,
        token,
        limit: Arc::new(Semaphore::new(concurrency)),
    };

    let app = Router::new()
        .route("/tts", any(tts))
        .route("/voices", get(voices))
        .route("/health", get(|| async { "ok" }))
        .with_state(state);

    let addr = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("cannot bind {addr}: {e}"));
    println!("edge-tts-legado listening on http://{addr}");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_punctuation_only() {
        assert!(split_text("……\n———").is_empty());
        assert!(split_text("   ").is_empty());
    }

    #[test]
    fn escapes_and_keeps_text() {
        assert_eq!(split_text("A&B<"), vec!["A&amp;B&lt;".to_string()]);
    }

    #[test]
    fn long_text_is_split_by_bytes() {
        let s = "你好世界。".repeat(1000); // 5 字 * 3 字节 * 1000 = 15000 字节
        let chunks = split_text(&s);
        assert!(chunks.len() >= 5);
        assert!(chunks.iter().all(|c| c.len() <= CHUNK_HARD_BYTES));
        assert_eq!(chunks.concat(), s);
        // 没有句末标点的超长文本也不能超过硬上限
        let t = "字".repeat(5000);
        assert!(split_text(&t).iter().all(|c| c.len() <= CHUNK_HARD_BYTES));
    }
}
