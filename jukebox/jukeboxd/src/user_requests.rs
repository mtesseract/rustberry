use failure::Fallible;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use slog_scope::{error, info};
use std::env;
use std::fmt::Display;
use std::io::BufRead;
use std::sync::mpsc;
use std::sync::mpsc::{Receiver, Sender};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum UserRequest {
    SpotifyUri(String),
}

mod tests {
    use super::*;
    #[test]
    fn test_user_request_spotify_uri_serialization() {
        let user_req = UserRequest::SpotifyUri("foo".to_string());
        let serialized = serde_json::to_string(&user_req).unwrap();
        assert_eq!(serialized, "".to_string());
    }

}

pub trait UserRequestTransmitterBackend<T: DeserializeOwned> {
    fn run(&mut self, tx: Sender<Option<T>>) -> Fallible<()>;
}

pub struct UserRequests<T>
where
    T: Sync + Send + 'static,
{
    rx: Receiver<Option<T>>,
}

pub struct UserRequestsTransmitter<
    T: DeserializeOwned + std::fmt::Debug,
    TB: UserRequestTransmitterBackend<T>,
> {
    backend: TB,
    first_req: Option<T>,
}

impl<T: std::fmt::Debug + DeserializeOwned + Clone, TB: UserRequestTransmitterBackend<T>>
    UserRequestsTransmitter<T, TB>
{
    pub fn new(backend: TB) -> Fallible<Self> {
        let first_req = match env::var("FIRST_REQUEST") {
            Ok(first_req) => match serde_json::from_str(&first_req) {
                Ok(first_req) => Some(first_req),
                Err(err) => {
                    error!(
                        "Failed to deserialize first request '{}': {}",
                        first_req, err
                    );
                    None
                }
            },
            Err(env::VarError::NotPresent) => None,
            Err(err) => {
                error!("Failed to retrieve FIRST_REQUEST: {}", err.to_string());
                None
            }
        };
        Ok(UserRequestsTransmitter { backend, first_req })
    }

    pub fn run(&mut self, tx: Sender<Option<T>>) -> Fallible<()> {
        if let Some(ref first_req) = self.first_req {
            let first_req = (*first_req).clone();
            info!(
                "Automatically transmitting first user request: {:?}",
                &first_req
            );
            if let Err(err) = tx.send(Some(first_req.clone())) {
                error!("Failed to transmit first request {:?}: {}", first_req, err);
            }
        }
        self.backend.run(tx)
    }
}

pub mod stdin {
    use super::*;

    pub struct UserRequestTransmitterStdin<T> {
        _phantom: Option<T>,
    }

    impl<T: DeserializeOwned + std::fmt::Debug> UserRequestTransmitterStdin<T> {
        pub fn new() -> Fallible<Self> {
            Ok(UserRequestTransmitterStdin { _phantom: None })
        }
    }

    impl<T: DeserializeOwned + PartialEq + Clone> UserRequestTransmitterBackend<T>
        for UserRequestTransmitterStdin<T>
    {
        fn run(&mut self, tx: Sender<Option<T>>) -> Fallible<()> {
            let mut last: Option<T> = None;

            let stdin = std::io::stdin();
            for line in stdin.lock().lines() {
                if let Ok(ref line) = line {
                    let req = if line == "" {
                        None
                    } else {
                        Some(serde_json::from_str(line).unwrap())
                    };
                    if last != req {
                        tx.send(req.clone()).unwrap();
                    }
                    last = req;
                }
            }

            panic!();
        }
    }
}

pub mod rfid {
    use super::*;

    use spidev::{SpiModeFlags, Spidev, SpidevOptions};
    use std::io;

    use crate::rfid::*;

    // use rfid_rs::{picc, MFRC522};

    pub struct UserRequestTransmitterRfid<T> {
        picc: RfidController,
        _phantom: Option<T>,
    }

    impl<T: DeserializeOwned + std::fmt::Debug> UserRequestTransmitterRfid<T> {
        pub fn new() -> Fallible<Self> {
            let picc = RfidController::new()?;

            Ok(UserRequestTransmitterRfid {
                picc,
                _phantom: None,
            })
        }
    }

    impl<T: DeserializeOwned + PartialEq + Clone> UserRequestTransmitterBackend<T>
        for UserRequestTransmitterRfid<T>
    {
        fn run(&mut self, tx: Sender<Option<T>>) -> Fallible<()> {
            let mut last_uid: Option<String> = None;

            loop {
                match self.picc.open_tag() {
                    Err(err) => {
                        error!("Failed to open RFID tag: {}", err);
                    }
                    Ok(None) => {
                        if last_uid.is_some() {
                            info!("RFID Tag gone");
                            last_uid = None;
                            tx.send(None).expect("tx send");
                        }
                    }
                    Ok(Some(tag)) => {
                        let current_uid = format!("{:?}", tag.uid);
                        if last_uid != Some(current_uid.clone()) {
                            // new tag!
                            let mut tag_reader = tag.new_reader();
                            match tag_reader.read_string() {
                                Ok(s) => {
                                    let req: T = serde_json::from_str(&s)
                                        .expect("Deserializing user request");
                                    tx.send(Some(req.clone())).expect("tx send");
                                }
                                Err(err) => {
                                    error!(
                                        "Failed to retrieve data from RFID Tag {}: {}",
                                        &current_uid, err
                                    );
                                }
                            }
                            last_uid = Some(current_uid);
                        }
                    }
                }
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
        }
    }
}

impl<T: DeserializeOwned + Clone + PartialEq + Sync + Send + 'static> UserRequests<T> {
    pub fn new<TX>(mut transmitter: UserRequestsTransmitter<T, TX>) -> Self
    where
        TX: Send + 'static + UserRequestTransmitterBackend<T>,
        T: DeserializeOwned + std::fmt::Debug,
    {
        let (tx, rx): (Sender<Option<T>>, Receiver<Option<T>>) = mpsc::channel();
        std::thread::spawn(move || transmitter.run(tx));
        Self { rx }
    }
}

impl<T: Sync + Send + 'static> Iterator for UserRequests<T> {
    type Item = Option<T>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.rx.recv() {
            Ok(next_item) => Some(next_item),
            Err(err) => {
                error!(
                    "Failed to receive next user request from UserRequestsTransmitter: {}",
                    err
                );
                None
            }
        }
    }
}
