//! One entry point for every login Settings ▸ Accounts offers.
//!
//! [`Logins::start`] begins a provider's login and returns a [`LoginSession`]; its
//! [`LoginSession::prompt`] is what the login screen shows (a browser address to open, or a device
//! code and where to enter it), and [`LoginSession::finish`] waits for you and returns the
//! [`Credential`] to store. The caller stores it under the account's [`crate::CredentialRef`] and
//! clears that key in [`crate::LoginHealth`]. API keys don't need a session: see
//! [`crate::api_key`].
//!
//! Each provider's login is its own module with its own endpoints and errors, so one breaking
//! touches no other. Every endpoint is in [`LoginEndpoints`], so they can all be pointed at local
//! fake servers.

use crate::api_key::ApiKeyEndpoints;
use crate::cancel::Cancel;
use crate::chatgpt::{self, ChatGptEndpoints, ChatGptFlow, ChatGptLogin, PendingLogin};
use crate::copilot::{self, CopilotEndpoints};
use crate::device_code::DeviceCode;
use crate::kimi::{self, KimiEndpoints};
use crate::openrouter::{self, OpenRouterEndpoints, PendingOpenRouter};
use crate::xai::{self, XaiEndpoints};
use crate::{AuthError, Credential};
use qm_types::domain::Provider;
use std::time::Duration;
use uuid::Uuid;

/// How long a browser login waits for you.
pub const BROWSER_TIMEOUT: Duration = Duration::from_mins(5);

/// How an account is connected, for the "Connect an account" list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoginMethod {
    /// A login in your browser that calls back to the app (ChatGPT, OpenRouter).
    Browser,
    /// A code you enter in your browser (xAI, Copilot, Kimi).
    DeviceCode,
    /// Found on this Mac, not logged into here (Claude, through your `claude` login).
    Detected,
    /// A pasted API key.
    ApiKey,
}

/// The subscription logins, in the order the login screen lists them, with their method.
pub const SUBSCRIPTION_LOGINS: &[(Provider, LoginMethod)] = &[
    (Provider::Claude, LoginMethod::Detected),
    (Provider::ChatGpt, LoginMethod::Browser),
    (Provider::Copilot, LoginMethod::DeviceCode),
    (Provider::Xai, LoginMethod::DeviceCode),
    (Provider::Kimi, LoginMethod::DeviceCode),
    (Provider::OpenRouter, LoginMethod::Browser),
];

/// The name a provider's login shows.
pub fn display_name(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "Claude",
        Provider::ChatGpt => "ChatGPT",
        Provider::Copilot => "GitHub Copilot",
        Provider::Xai => "Grok (xAI)",
        Provider::Kimi => "Kimi",
        Provider::OpenRouter => "OpenRouter",
        Provider::Gemini => "Gemini",
        Provider::Anthropic => "Anthropic",
        Provider::OpenAi => "OpenAI",
        Provider::Custom => "Custom endpoint",
        Provider::Scripted => "Scripted",
    }
}

/// Every endpoint every login uses. [`Default`] is the real providers'.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LoginEndpoints {
    pub chatgpt: ChatGptEndpoints,
    /// Which ChatGPT login [`Logins::start`] uses: Sign in with ChatGPT, or the Codex login as the
    /// fallback.
    pub chatgpt_flow: ChatGptFlowChoice,
    pub xai: XaiEndpoints,
    pub copilot: CopilotEndpoints,
    pub kimi: KimiEndpoints,
    pub openrouter: OpenRouterEndpoints,
    pub api_keys: ApiKeyEndpoints,
}

/// Which ChatGPT login to run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ChatGptFlowChoice {
    /// OpenAI's official Sign in with ChatGPT (the primary).
    #[default]
    SignInWithChatGpt,
    /// The Codex login in the browser (the fallback).
    Codex,
    /// The Codex login by device code (the fallback, when the browser can't reach port 1455).
    CodexDeviceCode,
}

/// Starts logins.
#[derive(Debug, Clone)]
pub struct Logins {
    endpoints: LoginEndpoints,
    install_id: Uuid,
    http: reqwest::Client,
}

/// What the login screen shows while a login waits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoginPrompt {
    /// Open this address in the browser (the app opens it, and shows it too).
    Browser { url: String },
    /// Open `url` and enter `code`.
    DeviceCode { url: String, code: String },
}

/// A login waiting for you.
#[derive(Debug)]
pub struct LoginSession {
    provider: Provider,
    prompt: LoginPrompt,
    started: tokio::time::Instant,
    expires_in: Duration,
    pending: Pending,
    http: reqwest::Client,
    endpoints: LoginEndpoints,
}

#[derive(Debug)]
enum Pending {
    ChatGpt(Box<PendingLogin>),
    OpenRouter(PendingOpenRouter),
    Device(DeviceCode),
}

impl Logins {
    /// `install_id` is this installation's host ID ([`chatgpt::install_id`]).
    pub fn new(endpoints: LoginEndpoints, install_id: Uuid) -> Self {
        Self {
            endpoints,
            install_id,
            http: crate::http::client(),
        }
    }

    pub fn endpoints(&self) -> &LoginEndpoints {
        &self.endpoints
    }

    /// The HTTP client logins use (for [`crate::api_key::check`]).
    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// Starts `provider`'s subscription login. Claude isn't logged into here (its login is
    /// detected), and API-key providers take a key instead: both are refused.
    pub async fn start(&self, provider: Provider) -> Result<LoginSession, AuthError> {
        let e = &self.endpoints;
        let (pending, expires_in) = match provider {
            Provider::ChatGpt => match e.chatgpt_flow {
                ChatGptFlowChoice::SignInWithChatGpt | ChatGptFlowChoice::Codex => {
                    let flow = if e.chatgpt_flow == ChatGptFlowChoice::Codex {
                        ChatGptFlow::Codex
                    } else {
                        ChatGptFlow::SignInWithChatGpt
                    };
                    let login = ChatGptLogin::new(flow, e.chatgpt.clone(), self.install_id);
                    (
                        Pending::ChatGpt(Box::new(login.start().await?)),
                        BROWSER_TIMEOUT,
                    )
                }
                ChatGptFlowChoice::CodexDeviceCode => {
                    let device = chatgpt::start_codex_device(&self.http, &e.chatgpt).await?;
                    let expires_in = device.expires_in;
                    (Pending::Device(device), expires_in)
                }
            },
            Provider::OpenRouter => (
                Pending::OpenRouter(openrouter::start(&self.http, &e.openrouter).await?),
                BROWSER_TIMEOUT,
            ),
            Provider::Xai => device(xai::start(&self.http, &e.xai).await?),
            Provider::Copilot => device(copilot::start(&self.http, &e.copilot).await?),
            Provider::Kimi => device(kimi::start(&self.http, &e.kimi).await?),
            other => {
                return Err(AuthError::Login(format!(
                    "{} has no login to start here",
                    display_name(other)
                )));
            }
        };
        let prompt = match &pending {
            Pending::ChatGpt(p) => LoginPrompt::Browser {
                url: p.authorize_url().to_owned(),
            },
            Pending::OpenRouter(p) => LoginPrompt::Browser {
                url: p.authorize_url().to_owned(),
            },
            Pending::Device(d) => LoginPrompt::DeviceCode {
                url: d.open_url().to_owned(),
                code: d.user_code.clone(),
            },
        };
        Ok(LoginSession {
            provider,
            prompt,
            started: tokio::time::Instant::now(),
            expires_in,
            pending,
            http: self.http.clone(),
            endpoints: self.endpoints.clone(),
        })
    }
}

fn device(device: DeviceCode) -> (Pending, Duration) {
    let expires_in = device.expires_in;
    (Pending::Device(device), expires_in)
}

impl LoginSession {
    pub fn provider(&self) -> Provider {
        self.provider
    }

    /// What to show: the address to open, or the code and where to enter it.
    pub fn prompt(&self) -> &LoginPrompt {
        &self.prompt
    }

    /// How long until the code or the wait runs out (zero once it has).
    pub fn expires_in(&self) -> Duration {
        self.expires_in.saturating_sub(self.started.elapsed())
    }

    /// Waits for you to finish in the browser; ends early with [`AuthError::Cancelled`] when
    /// `cancel` is pressed, or [`AuthError::Expired`] when time runs out.
    pub async fn finish(self, cancel: &Cancel) -> Result<Credential, AuthError> {
        let remaining = self.expires_in();
        let e = &self.endpoints;
        match self.pending {
            Pending::ChatGpt(p) => p.finish_or_cancel(remaining, cancel).await,
            Pending::OpenRouter(p) => p.finish(remaining, cancel).await,
            Pending::Device(d) => match self.provider {
                Provider::Xai => xai::finish(&self.http, &e.xai, &d, cancel).await,
                Provider::Copilot => copilot::finish(&self.http, &e.copilot, &d, cancel).await,
                Provider::Kimi => kimi::finish(&self.http, &e.kimi, &d, cancel).await,
                Provider::ChatGpt => {
                    chatgpt::finish_codex_device(&self.http, &e.chatgpt, &d, cancel).await
                }
                other => Err(AuthError::Login(format!(
                    "{} has no device login",
                    display_name(other)
                ))),
            },
        }
    }

    /// Finishes a browser login with the address the browser ended on, pasted by you (when the
    /// browser can't reach this Mac's callback port). Device-code logins refuse it.
    pub async fn finish_with_pasted(self, pasted: &str) -> Result<Credential, AuthError> {
        match self.pending {
            Pending::ChatGpt(p) => p.finish_with_pasted_url(pasted).await,
            Pending::OpenRouter(p) => p.finish_with_pasted(pasted).await,
            Pending::Device(_) => Err(AuthError::Login(
                "this login uses a code, not a pasted address".into(),
            )),
        }
    }
}
