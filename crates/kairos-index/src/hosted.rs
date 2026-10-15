//! The hosted summarizer (KAIROS-T-0341, COLLIERY-I-0611): an
//! OpenAI-compatible chat endpoint. Ollama Cloud speaks it, and so do a
//! local Ollama, vLLM and OpenRouter, so one client serves them all.
//!
//! The request is `POST {base}/chat/completions` with the system prompt of
//! [`crate::summary::SYSTEM_PROMPT`] and the user prompt of
//! [`SummaryRequest::prompt`], temperature 0, a cap on the tokens of the
//! answer. The answer is `choices[0].message.content`.
//!
//! A hosted endpoint takes many requests at once, so [`HostedChat`] sends a
//! batch on `concurrency` threads ([`Summarizer::summarize_many`]). The
//! `concurrency` is also the most requests in flight to the endpoint (the
//! base URL and the model) from this process, for all builds together
//! (KAIROS-T-0495, [`crate::rate`]). A 429 or a 503 waits and is tried
//! again, up to 6 tries, after the `Retry-After` of the answer or a wait
//! that doubles from 2 s; another 5xx or no answer is tried again 3 times.
//! The final error names the status and the first line of the body. The
//! client is blocking, as the embedding providers are: the builder calls
//! it from a blocking worker, never from an async task.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde::Deserialize;

use crate::rate::{Attempt, Backoff, Beat, Limit, limit_for, parse_retry_after, send};
use crate::summary::{SYSTEM_PROMPT, Summarizer, SummaryRequest};

/// How to reach the endpoint.
#[derive(Debug, Clone)]
pub struct HostedConfig {
    /// The provider, for the model name of the summaries: `ollama-cloud`
    /// for every OpenAI-compatible endpoint.
    pub provider: String,
    /// The base URL, without `/chat/completions`: for example
    /// `https://ollama.com/v1`.
    pub base_url: String,
    /// The model to ask for, as the endpoint names it.
    pub model: String,
    /// The bearer token, if the endpoint wants one.
    pub api_key: Option<String>,
    /// The requests sent at the same time, 1 or more.
    pub concurrency: usize,
    /// The time for one request.
    pub timeout: Duration,
    /// The waits after a refusal for the rate (KAIROS-T-0495).
    pub backoff: Backoff,
}

impl HostedConfig {
    /// A configuration with the default timeout of 120 seconds.
    pub fn new(provider: &str, base_url: &str, model: &str, concurrency: usize) -> Self {
        HostedConfig {
            provider: provider.to_string(),
            base_url: base_url.to_string(),
            model: model.to_string(),
            api_key: None,
            concurrency: concurrency.max(1),
            timeout: Duration::from_secs(120),
            backoff: Backoff::default(),
        }
    }
}

/// The most tokens of one summary. A summary is 1 to 3 sentences; this cap
/// stops a model that does not stop.
const MAX_TOKENS: u32 = 400;

/// Summaries from an OpenAI-compatible chat endpoint.
pub struct HostedChat {
    client: reqwest::blocking::Client,
    config: HostedConfig,
    /// The cap of requests in flight to the endpoint, shared by each client
    /// of the endpoint in this process.
    limit: Arc<Limit>,
    /// The progress of the build, while a request waits (KAIROS-T-0495).
    beat: Option<Beat>,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: Message,
}

#[derive(Deserialize)]
struct Message {
    #[serde(default)]
    content: String,
}

impl HostedChat {
    /// A client for `config`. It sends nothing until the first summary.
    pub fn new(config: HostedConfig) -> Result<Self, String> {
        let client = reqwest::blocking::Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(|e| format!("the HTTP client did not start: {e}"))?;
        let limit = limit_for(
            &format!("{}|{}", config.base_url.trim_end_matches('/'), config.model),
            config.concurrency,
        );
        Ok(HostedChat {
            client,
            config,
            limit,
            beat: None,
        })
    }

    fn url(&self) -> String {
        format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        )
    }

    /// One request, tried again as [`crate::rate`] says.
    fn request(&self, prompt: &str) -> Result<String, String> {
        let url = self.url();
        let body = serde_json::json!({
            "model": self.config.model,
            "messages": [
                {"role": "system", "content": SYSTEM_PROMPT},
                {"role": "user", "content": prompt},
            ],
            "temperature": 0,
            "max_tokens": MAX_TOKENS,
            "stream": false,
        });
        send(
            &self.limit,
            &self.config.backoff,
            self.beat.as_ref(),
            || {
                let mut req = self.client.post(&url).json(&body);
                if let Some(key) = &self.config.api_key {
                    req = req.bearer_auth(key);
                }
                let response = match req.send() {
                    Ok(response) => response,
                    Err(e) => return Attempt::Failed(format!("{url}: {e}")),
                };
                let status = response.status();
                if !status.is_success() {
                    let retry_after = response
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| parse_retry_after(v, SystemTime::now()));
                    let text = response.text().unwrap_or_default();
                    let message = format!(
                        "{url} answered {status}: {}",
                        text.lines().next().unwrap_or("")
                    );
                    return match status.as_u16() {
                        429 | 503 => Attempt::Limited {
                            message,
                            retry_after,
                        },
                        _ if status.is_server_error() => Attempt::Failed(message),
                        _ => Attempt::Done(Err(message)),
                    };
                }
                let parsed: ChatResponse = match response.json() {
                    Ok(parsed) => parsed,
                    Err(e) => {
                        return Attempt::Done(Err(format!(
                            "{url}: the answer is not a chat completion: {e}"
                        )));
                    }
                };
                let content = parsed
                    .choices
                    .into_iter()
                    .next()
                    .map(|c| c.message.content)
                    .unwrap_or_default();
                if content.trim().is_empty() {
                    return Attempt::Done(Err(format!("{url}: the answer has no content")));
                }
                Attempt::Done(Ok(content))
            },
        )
    }
}

impl Summarizer for HostedChat {
    fn model(&self) -> String {
        format!("{}/{}", self.config.provider, self.config.model)
    }

    fn summarize(&mut self, request: &SummaryRequest) -> Result<String, String> {
        self.request(&request.prompt())
    }

    fn concurrency(&self) -> usize {
        self.config.concurrency.max(1)
    }

    fn heartbeat(&mut self, beat: Beat) {
        self.beat = Some(beat);
    }

    /// The requests on `concurrency` threads at the same time; the results
    /// in the order of the requests.
    fn summarize_many(&mut self, requests: &[SummaryRequest]) -> Vec<Result<String, String>> {
        let prompts: Vec<String> = requests.iter().map(|r| r.prompt()).collect();
        in_parallel(&prompts, |prompt| self.request(prompt))
    }
}

/// Run `request` for each prompt on its own thread, and give the results
/// in the order of the prompts. The caller gives one batch at a time, of
/// the size of its concurrency, so the thread count is bounded.
pub(crate) fn in_parallel<F>(prompts: &[String], request: F) -> Vec<Result<String, String>>
where
    F: Fn(&str) -> Result<String, String> + Sync,
{
    let request = &request;
    std::thread::scope(|scope| {
        let handles: Vec<_> = prompts
            .iter()
            .map(|prompt| scope.spawn(move || request(prompt)))
            .collect();
        handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .unwrap_or_else(|_| Err("the request thread stopped".to_string()))
            })
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_model_name_has_the_provider() {
        let chat = HostedChat::new(HostedConfig::new(
            "ollama-cloud",
            "https://ollama.com/v1/",
            "gemma4:31b",
            4,
        ))
        .unwrap();
        assert_eq!(chat.model(), "ollama-cloud/gemma4:31b");
        assert_eq!(chat.url(), "https://ollama.com/v1/chat/completions");
        assert_eq!(chat.concurrency(), 4);
    }

    #[test]
    fn the_concurrency_is_at_least_one() {
        let chat = HostedChat::new(HostedConfig::new("ollama-cloud", "http://x", "m", 0)).unwrap();
        assert_eq!(chat.concurrency(), 1);
    }

    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::Mutex;

    /// A fake chat endpoint on an ephemeral port (KAIROS-T-0495): it gives
    /// the answers of `script` in order, then the last one for each later
    /// request. An answer is `(status, Retry-After)`; a 200 is a chat
    /// completion. It closes each connection after its answer.
    fn serve(script: Vec<(u16, Option<&'static str>)>) -> (String, Arc<Mutex<usize>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let count = Arc::new(Mutex::new(0usize));
        let seen = Arc::clone(&count);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut length = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    let lower = line.to_ascii_lowercase();
                    if let Some(v) = lower.strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0u8; length];
                let _ = reader.read_exact(&mut body);
                let n = {
                    let mut c = seen.lock().unwrap();
                    *c += 1;
                    *c
                };
                let (status, retry_after) = script[(n - 1).min(script.len() - 1)];
                let (reason, body) = if status == 200 {
                    (
                        "OK",
                        r#"{"choices":[{"message":{"content":"A summary."}}]}"#.to_string(),
                    )
                } else {
                    (
                        "Refused",
                        r#"{"error":"too many concurrent requests"}"#.to_string(),
                    )
                };
                let mut head = format!(
                    "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n",
                    body.len()
                );
                if let Some(value) = retry_after {
                    head.push_str(&format!("retry-after: {value}\r\n"));
                }
                let _ = stream.write_all(format!("{head}\r\n{body}").as_bytes());
            }
        });
        (format!("http://{addr}/v1"), count)
    }

    /// A client of `base` whose waits are recorded, not slept.
    fn client(base: &str, model: &str) -> (HostedChat, Arc<Mutex<Vec<Duration>>>) {
        let waits = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&waits);
        let mut config = HostedConfig::new("ollama-cloud", base, model, 1);
        config.backoff.sleep = Arc::new(move |d, _: &dyn Fn()| seen.lock().unwrap().push(d));
        (HostedChat::new(config).unwrap(), waits)
    }

    #[test]
    fn a_429_with_and_without_retry_after_waits_then_gets_the_summary() {
        let (base, count) = serve(vec![
            (429, Some("7")),
            (429, None),
            (503, None),
            (200, None),
        ]);
        let (chat, waits) = client(&base, "backoff-ok");
        assert_eq!(chat.request("p").as_deref(), Ok("A summary."));
        assert_eq!(*count.lock().unwrap(), 4);
        assert_eq!(
            *waits.lock().unwrap(),
            [7, 4, 8].map(Duration::from_secs),
            "the Retry-After, then 4 s and 8 s for the 2nd and 3rd refusal"
        );
    }

    #[test]
    fn a_429_always_gives_a_rate_limit_error_after_6_tries() {
        let (base, count) = serve(vec![(429, None)]);
        let (chat, waits) = client(&base, "backoff-always");
        let error = chat.request("p").unwrap_err();
        assert!(crate::rate::is_rate_limited(&error), "{error}");
        assert!(error.contains("429"), "{error}");
        assert_eq!(*count.lock().unwrap(), 6);
        assert_eq!(waits.lock().unwrap().len(), 5);
    }

    #[test]
    fn a_401_is_not_tried_again() {
        let (base, count) = serve(vec![(401, None)]);
        let (chat, waits) = client(&base, "backoff-401");
        let error = chat.request("p").unwrap_err();
        assert!(!crate::rate::is_rate_limited(&error), "{error}");
        assert_eq!(*count.lock().unwrap(), 1);
        assert!(waits.lock().unwrap().is_empty());
    }
}
