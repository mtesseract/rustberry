use std::fmt::{self, Display};
use std::thread::{self, JoinHandle};

use crate::access_token_provider::{self, AccessTokenProvider, AtpError};

use hyper::header::AUTHORIZATION;
use reqwest::Client;
use serde::Serialize;
use slog_scope::{error, info, warn};
use std::convert::From;
use std::sync::{Arc, RwLock};

use crossbeam_channel::{Receiver, RecvError, RecvTimeoutError, Select, Sender};

#[derive(Debug)]
pub enum Error {
    HTTP(reqwest::Error),
}

impl Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::HTTP(err) => write!(f, "Spotify HTTP Error {}", err),
        }
    }
}

impl From<reqwest::Error> for Error {
    fn from(err: reqwest::Error) -> Self {
        Error::HTTP(err)
    }
}

impl std::error::Error for Error {}

pub struct SpotifyPlayer {
    access_token_provider: AccessTokenProvider,
    http_client: Client,
}

#[derive(Debug, Clone, Serialize)]
struct StartPlayback {
    #[serde(skip_serializing_if = "Option::is_none")]
    context_uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    uris: Option<Vec<String>>,
}

impl SpotifyPlayer {
    pub fn new(access_token_provider: AccessTokenProvider) -> Self {
        let http_client = Client::new();

        let player = SpotifyPlayer {
            access_token_provider,
            http_client,
        };

        player
    }

    fn derive_start_playback_payload_from_spotify_uri(spotify_uri: &str) -> StartPlayback {
        if &spotify_uri[0..14] == "spotify:album:" {
            StartPlayback {
                uris: None,
                context_uri: Some(spotify_uri.clone().to_string()),
            }
        } else {
            StartPlayback {
                uris: Some(vec![spotify_uri.clone().to_string()]),
                context_uri: None,
            }
        }
    }

    pub fn start_playback(
        &self,
        access_token: &str,
        device_id: &str,
        spotify_uri: &str,
    ) -> Result<(), Error> {
        let msg = "Failed to start Spotify playback";
        let req = Self::derive_start_playback_payload_from_spotify_uri(spotify_uri);
        self.http_client
            .put("https://api.spotify.com/v1/me/player/play")
            .query(&[("device_id", &device_id)])
            .header(AUTHORIZATION, access_token)
            .json(&req)
            .send()
            .map_err(|err| {
                error!("{}: Executing HTTP request failed: {}", msg, err);
                err
            })
            .map(|mut rsp| {
                if !rsp.status().is_success() {
                    error!("{}: HTTP Failure {}: {:?}", msg, rsp.status(), rsp.text());
                }
                rsp
            })?
            .error_for_status()
            .map(|_| ())
            .map_err(|err| Error::HTTP(err))
    }

    pub fn stop_playback(&self, access_token: &str, device_id: &str) -> Result<(), Error> {
        let msg = "Failed to stop Spotify playback";
        self.http_client
            .put("https://api.spotify.com/v1/me/player/pause")
            .query(&[("device_id", &device_id)])
            .body("")
            .header(AUTHORIZATION, access_token)
            .send()
            .map_err(|err| {
                error!("{}: Executing HTTP request failed: {}", msg, err);
                err
            })
            .map(|mut rsp| {
                if !rsp.status().is_success() {
                    error!("{}: HTTP Failure {}: {:?}", msg, rsp.status(), rsp.text());
                }
                rsp
            })?
            .error_for_status()
            .map(|_| ())
            .map_err(|err| Error::HTTP(err))
    }
}
