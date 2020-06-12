use std::sync::Arc;
use std::time::Duration;

use tokio::stream::StreamExt;
use tokio::sync::mpsc::{channel, Receiver};

// use crossbeam_channel::{self, Receiver, Select};
use failure::Fallible;
use slog::{self, o, Drain};
use slog_async;
use slog_scope::{error, info, warn};
use slog_term;

use futures::future::AbortHandle;
use futures_util::TryFutureExt;
use rustberry::config::Config;
use rustberry::effects::{test::TestInterpreter, Interpreter, ProdInterpreter};
use rustberry::input_controller::{
    button, mock, playback, Input, InputSource, InputSourceFactory, ProdInputSource,
    ProdInputSourceFactory,
};
use rustberry::player::{self, PlaybackRequest, Player};

use led::Blinker;

type DynInterpreter = Arc<Box<dyn Interpreter + Sync + Send + 'static>>;

fn main() -> Fallible<()> {
    let decorator = slog_term::TermDecorator::new().build();
    let drain = slog_term::FullFormat::new(decorator).build().fuse();
    let drain = slog_async::Async::new(drain).build().fuse();
    let logger = slog::Logger::root(drain, o!());
    let _guard = slog_scope::set_global_logger(logger);

    slog_scope::scope(&slog_scope::logger().new(o!()), || main_with_log())
}

async fn create_mock_meta_app(config: Config) -> Fallible<MetaApp> {
    warn!("Creating Mock Application");

    let isf = Box::new(mock::MockInputSourceFactory::new()?)
        as Box<dyn InputSourceFactory + Sync + Send + 'static>;

    let (interpreter, interpreted_effects) = TestInterpreter::new();
    let interpreter =
        Arc::new(Box::new(interpreter) as Box<dyn Interpreter + Sync + Send + 'static>);

    let blinker = Blinker::new(interpreter.clone()).unwrap();

    let _handle = std::thread::Builder::new()
        .name("mock-effect-interpreter".to_string())
        .spawn(move || {
            for eff in interpreted_effects.iter() {
                info!("Mock interpreter received effect: {:?}", eff);
            }
        })
        .unwrap();

    let application = MetaApp::new(config, interpreter, blinker, isf)
        .await
        .unwrap();

    Ok(application)
}

async fn create_production_meta_app(config: Config) -> Fallible<MetaApp> {
    info!("Creating Production Application");
    // Create Effects Channel and Interpreter.
    let interpreter = ProdInterpreter::new(&config).unwrap();
    let interpreter: Arc<Box<dyn Interpreter + Sync + Send + 'static>> =
        Arc::new(Box::new(interpreter));

    let blinker = Blinker::new(interpreter.clone()).unwrap();
    blinker
        .run_async(led::Cmd::Loop(Box::new(led::Cmd::Many(vec![
            led::Cmd::On(Duration::from_millis(100)),
            led::Cmd::Off(Duration::from_millis(100)),
        ]))))
        .await;

    interpreter.wait_until_ready().map_err(|err| {
        error!("Failed to wait for interpreter readiness: {}", err);
        err
    })?;

    // Prepare individual input channels.
    // let button_controller_handle =
    //     button::cdev_gpio::CdevGpio::new_from_env(|cmd| Some(Input::Button(cmd)))?;
    let playback_controller_handle =
        playback::rfid::PlaybackRequestTransmitterRfid::new(|req| Some(Input::Playback(req)))?;

    let mut isf = ProdInputSourceFactory::new()?;
    isf.with_buttons(Box::new(|| {
        button::cdev_gpio::CdevGpio::new_from_env(|cmd| Some(cmd)).unwrap()
    }));
    isf.with_playback(Box::new(|| {
        playback::rfid::PlaybackRequestTransmitterRfid::new(|cmd| Some(cmd)).unwrap()
    }));

    let mut application = MetaApp::new(config, interpreter, blinker, Box::new(isf)).await?;

    Ok(application)
}

fn main_with_log() -> Fallible<()> {
    let config = envy::from_env::<Config>()?;
    info!("Configuration"; o!("device_name" => &config.device_name));

    // let mut runtime = tokio::runtime::Runtime::new().unwrap();
    let mut runtime = tokio::runtime::Builder::new()
        .threaded_scheduler()
        .enable_all()
        .build()?;

    // let mut application = create_production_meta_app(config, runtime.handle())?;

    runtime.block_on(async move {
        let application = if std::env::var("MOCK_MODE")
            .map(|x| x == "YES")
            .unwrap_or(false)
        {
            create_mock_meta_app(config).await?
        } else {
            create_production_meta_app(config).await?
        };

        dbg!("about to block on application");
        application
            .run()
            .map_err(|err| {
                warn!("Jukebox loop terminated, terminating application: {}", err);
                err
            })
            .await
    });

    dbg!("application temrinated");
    Ok(())
}

#[derive(Clone)]
struct App {
    config: Config,
    // player: player::PlayerHandle,
    interpreter: Arc<Box<dyn Interpreter + Sync + Send + 'static>>,
    input_source_factory: Arc<Box<dyn InputSourceFactory + Sync + Send + 'static>>,
    blinker: Blinker,
    // runtime: tokio::runtime::Handle,
}

// #[derive(Clone)]
struct MetaApp {
    config: Config,
    // runtime: tokio::runtime::Handle,
    control_rx: tokio::sync::mpsc::Receiver<AppControl>,
    control_tx: tokio::sync::mpsc::Sender<AppControl>,
    interpreter: DynInterpreter,
    // inputs: Vec<Receiver<Input>>,
    jukebox_app: App,
    blinker: Blinker,
    input_factory: Arc<Box<dyn InputSourceFactory + Sync + Send + 'static>>,
}

use std::convert::Infallible;
use warp::http::StatusCode;
use warp::Filter;

#[derive(Clone)]
struct MetaAppHandle {
    control_tx: tokio::sync::mpsc::Sender<AppControl>,
}

impl MetaAppHandle {
    async fn current_mode(&self) -> AppMode {
        let (os_tx, os_rx) = tokio::sync::oneshot::channel();
        let mut control_tx = self.control_tx.clone();
        control_tx
            .try_send(AppControl::RequestCurrentMode(os_tx))
            .unwrap(); // FIXME
        os_rx.await.unwrap()
    }

    async fn set_mode(&self, mode: AppMode) {
        let mut control_tx = self.control_tx.clone();
        control_tx.try_send(AppControl::SetMode(mode));
    }
}

impl MetaApp {
    pub fn handle(&self) -> MetaAppHandle {
        let control_tx = self.control_tx.clone();
        let meta_app_handle = MetaAppHandle { control_tx };
        meta_app_handle
    }

    pub async fn new(
        config: Config,
        interpreter: Arc<Box<dyn Interpreter + Sync + Send + 'static>>,
        blinker: Blinker,
        input_factory: Box<dyn InputSourceFactory + Sync + Send + 'static>,
    ) -> Fallible<Self> {
        let (control_tx, control_rx) = tokio::sync::mpsc::channel(1);
        let input_source_factory = Arc::new(input_factory);

        let jukebox_app = App::new(
            config.clone(),
            interpreter.clone(),
            blinker.clone(),
            input_source_factory.clone(),
        )
        .unwrap();

        let meta_app = MetaApp {
            control_rx,
            control_tx,
            config,
            // app,
            blinker,
            interpreter,
            input_factory: input_source_factory,
            jukebox_app,
        };
        Ok(meta_app)
    }

    async fn get_current_mode(
        meta_app_handle: MetaAppHandle,
    ) -> Result<impl warp::Reply, Infallible> {
        info!("get_current_mode()");

        let current_mode = meta_app_handle.current_mode().await;
        let current_mode: String = format!("{:?}", current_mode);

        Ok(warp::reply::json(&current_mode))
    }

    fn with_db(
        db: MetaAppHandle,
    ) -> impl Filter<Extract = (MetaAppHandle,), Error = std::convert::Infallible> + Clone {
        warp::any().map(move || db.clone())
    }

    async fn set_mode_admin(
        meta_app_handle: MetaAppHandle,
    ) -> Result<impl warp::Reply, Infallible> {
        info!("set_mode_admin()");

        meta_app_handle.set_mode(AppMode::Admin).await;
        Ok(StatusCode::OK)
    }

    async fn set_mode_jukebox(
        meta_app_handle: MetaAppHandle,
    ) -> Result<impl warp::Reply, Infallible> {
        info!("set_mode_jukebox()");

        meta_app_handle.set_mode(AppMode::Jukebox).await;
        Ok(StatusCode::OK)
    }

    async fn put_rfid_tag(meta_app_handle: MetaAppHandle) -> Result<impl warp::Reply, Infallible> {
        Ok(StatusCode::OK)
    }

    async fn get_rfid_tag(meta_app_handle: MetaAppHandle) -> Result<impl warp::Reply, Infallible> {
        use rustberry::components::rfid::RfidController;
        let mut rc = RfidController::new().unwrap();
        let tag = rc.open_tag().unwrap().unwrap();
        println!("{:?}", tag.uid);
        let mut tag_reader = tag.new_reader();
        let s = tag_reader.read_string().expect("read_string");
        let req: PlaybackRequest = serde_json::from_str(&s).expect("PlaybackRequest Deserialization");
        dbg!(&req);
        Ok(StatusCode::OK)
    }

    pub async fn run(mut self) -> Fallible<()> {
        let routes = {
            let meta_app_handle = self.handle();
            let hello = warp::path!("hello" / String).map(|name| format!("Hello, {}!", name));
            let ep_mode = {
                let meta_app_handle = meta_app_handle.clone();
                warp::path!("mode")
                    .and(Self::with_db(meta_app_handle))
                    .and_then(Self::get_current_mode)
            };
            let ep_mode_admin = {
                let meta_app_handle = meta_app_handle.clone();
                warp::path!("mode-admin")
                    .and(Self::with_db(meta_app_handle))
                    .and_then(Self::set_mode_admin)
            };
            let eps_admin = {
                warp::path!("rfid-tag").and(
                    (warp::put()
                        .and(Self::with_db(meta_app_handle.clone()))
                        .and_then(Self::put_rfid_tag))
                    .or(warp::get()
                        .and(Self::with_db(meta_app_handle.clone()).and_then(Self::get_rfid_tag))),
                )
            };
            let ep_mode_jukebox = {
                let meta_app_handle = meta_app_handle.clone();
                warp::path!("mode-jukebox")
                    .and(Self::with_db(meta_app_handle))
                    .and_then(Self::set_mode_jukebox)
            };
            (warp::get().and(hello.or(ep_mode).or(ep_mode_admin).or(ep_mode_jukebox)))
                .or(warp::path!("admin").and(eps_admin))
        };

        tokio::spawn(warp::serve(routes).run(([0, 0, 0, 0], 3030)));

        let mut current_mode = AppMode::Starting;
        let mut abortable = None;

        loop {
            let cmd = self.control_rx.recv().await.unwrap();
            info!("MetaApp Ctrl Cmd: {:?}", &cmd);
            match cmd {
                AppControl::RequestCurrentMode(os_tx) => {
                    os_tx.send(current_mode.clone());
                }

                AppControl::SetMode(mode) => {
                    // FIXME
                    info!("Shutting down mode {:?}", current_mode);
                    abortable.map(|x: AbortHandle| x.abort());
                    info!("Starting {:?} mode", mode);
                    let abortable_handle = match mode {
                        AppMode::Starting => None,
                        AppMode::Jukebox => {
                            let isf2 = self.input_factory.clone();
                            let blinker = self.blinker.clone();
                            let interpreter = self.interpreter.clone();
                            let config = self.config.clone();
                            let (f, abortable_handle) = futures::future::abortable(async move {
                                let input_source = isf2.consume().unwrap();
                                Self::run_jukebox(config, input_source, blinker, interpreter).await
                            });
                            tokio::spawn(f);
                            Some(abortable_handle)
                        }
                        AppMode::Admin => None,
                    };
                    current_mode = mode;
                    abortable = abortable_handle;
                }
            }
        }
    }

    pub async fn run_jukebox(
        config: Config,
        input_source: Box<dyn InputSource + Sync + Send + 'static>,
        blinker: Blinker,
        interpreter: DynInterpreter,
    ) -> Fallible<()> {
        info!("Running Jukebox App");
        let player = Player::new(interpreter.clone()).await?;
        blinker
            .run_async(led::Cmd::Repeat(
                1,
                Box::new(led::Cmd::Many(vec![
                    led::Cmd::On(Duration::from_secs(1)),
                    led::Cmd::Off(Duration::from_secs(0)),
                ])),
            ))
            .await;

        loop {
            warn!("app loop");
            let mut rx = input_source.receiver();
            let el = rx.recv().await;
            match el {
                Err(err) => {
                    // if err.is_empty() {
                    //     // If the operation turns out not to be ready, retry.
                    //     continue;
                    // } else {
                    //     error!("Failed to receive input event: {}", err);
                    // }
                    panic!()
                }
                Ok(input) => {
                    blinker.stop();
                    match input {
                        Input::Button(cmd) => match cmd {
                            button::Command::Shutdown => {
                                if let Err(err) = interpreter.generic_command(
                                    config
                                        .shutdown_command
                                        .clone()
                                        .unwrap_or("sudo shutdown -h now".to_string()),
                                ) {
                                    error!("Failed to execute shutdown command: {}", err);
                                }
                            }
                            button::Command::VolumeUp => {
                                if let Err(err) = interpreter.generic_command(
                                    config
                                        .volume_up_command
                                        .clone()
                                        .unwrap_or("amixer -q -M set PCM 10%+".to_string()),
                                ) {
                                    error!("Failed to increase volume: {}", err);
                                }
                            }
                            button::Command::VolumeDown => {
                                if let Err(err) = interpreter.generic_command(
                                    config
                                        .volume_down_command
                                        .clone()
                                        .unwrap_or("amixer -q -M set PCM 10%-".to_string()),
                                ) {
                                    error!("Failed to decrease volume: {}", err);
                                }
                            }
                        },
                        Input::Playback(request) => {
                            if let Err(err) = player.playback(request.clone()).await {
                                error!("Failed to execute playback request {:?}: {}", request, err);
                            }
                            match request {
                                PlaybackRequest::Start(_) => {
                                    let _ = interpreter.led_on();
                                }
                                PlaybackRequest::Stop => {
                                    let _ = interpreter.led_off();
                                }
                            }
                        }
                    }
                }
            };
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AppMode {
    Starting,
    Jukebox,
    Admin,
}

#[derive(Debug)]
enum AppControl {
    SetMode(AppMode),
    RequestCurrentMode(tokio::sync::oneshot::Sender<AppMode>),
}

impl App {
    pub fn new(
        config: Config,
        // runtime: tokio::runtime::Handle,
        interpreter: Arc<Box<dyn Interpreter + Sync + Send + 'static>>,
        blinker: Blinker,
        input_source_factory: Arc<Box<dyn InputSourceFactory + Sync + Send + 'static>>,
    ) -> Fallible<Self> {
        let app = Self {
            // runtime,
            config,
            // player,
            interpreter,
            input_source_factory,
            blinker,
        };
        Ok(app)
    }

    // pub async fn run(&self)-> Fallible<AbortHandle> {
    //     let isf2 = self.input_source_factory.clone();

    // }
}

#[cfg(test)]
mod test {
    use rustberry::config::Config;
    use rustberry::effects::{test::TestInterpreter, Effects};
    use rustberry::input_controller::{button, Input};

    use super::*;

    #[test]
    fn jukebox_can_be_shut_down() {
        let (interpreter, effects_rx) = TestInterpreter::new();
        let interpreter =
            Arc::new(Box::new(interpreter) as Box<dyn Interpreter + Send + Sync + 'static>);
        // let (effects_tx, effects_rx) = crossbeam_channel::bounded(10);
        let config: Config = Config {
            refresh_token: "token".to_string(),
            client_id: "client".to_string(),
            client_secret: "secret".to_string(),
            device_name: "device".to_string(),
            post_init_command: None,
            shutdown_command: None,
            volume_up_command: None,
            volume_down_command: None,
        };
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let blinker = Blinker::new(runtime.handle().clone(), interpreter.clone()).unwrap();
        let inputs = vec![Input::Button(button::Command::Shutdown)];
        let effects_expected = vec![Effects::GenericCommand("sudo shutdown -h now".to_string())];
        let (input_tx, input_rx) = channel(100); // FIXME
        let app = App::new(
            config,
            runtime.handle().clone(),
            interpreter,
            blinker,
            &vec![input_rx],
        )
        .unwrap();
        for input in inputs {
            input_tx.send(input).unwrap();
        }
        drop(input_tx);
        runtime.spawn(app.run());
        let produced_effects: Vec<Effects> = effects_rx.iter().collect();

        assert_eq!(produced_effects, effects_expected);
    }
}

mod led {
    use std::cell::RefCell;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::{Arc, RwLock};
    use std::time::Duration;

    use failure::Fallible;
    use futures::future::AbortHandle;
    use rustberry::effects::Interpreter;
    use slog_scope::{error, info};

    #[derive(Clone)]
    pub struct Blinker {
        interpreter: Arc<Box<dyn Send + Sync + 'static + Interpreter>>,
        abort_handle: Arc<RwLock<Option<AbortHandle>>>,
        // runtime: tokio::runtime::Handle,
    }

    #[derive(Debug, Clone)]
    pub enum Cmd {
        Repeat(u32, Box<Cmd>),
        Loop(Box<Cmd>),
        On(Duration),
        Off(Duration),
        Many(Vec<Cmd>),
    }

    impl Blinker {
        pub fn new(
            // runtime: tokio::runtime::Handle,
            interpreter: Arc<Box<dyn Send + Sync + 'static + Interpreter>>,
        ) -> Fallible<Self> {
            let abort_handle = Arc::new(RwLock::new(None));
            let blinker = Self {
                interpreter,
                abort_handle,
                // runtime,
            };
            Ok(blinker)
        }

        fn run(
            interpreter: Arc<Box<dyn Send + Sync + 'static + Interpreter>>,
            cmd: Cmd,
        ) -> Pin<Box<dyn Future<Output = ()> + Send>> {
            Box::pin(async move {
                info!("Inside Blinker::run()");
                match cmd {
                    Cmd::On(duration) => {
                        info!("Blinker switches on");
                        let _ = interpreter.led_on();
                        tokio::time::delay_for(duration).await;
                    }
                    Cmd::Off(duration) => {
                        info!("Blinker switches off");
                        let _ = interpreter.led_off();
                        tokio::time::delay_for(duration).await;
                    }
                    Cmd::Many(cmds) => {
                        info!("Blinker processes Many");
                        for cmd in &cmds {
                            Self::run(interpreter.clone(), cmd.clone()).await;
                        }
                    }
                    Cmd::Repeat(n, cmd) => {
                        info!("Blinker processes Repeat (n = {})", n);
                        for _i in 0..n {
                            Self::run(interpreter.clone(), (*cmd).clone()).await;
                        }
                    }
                    Cmd::Loop(cmd) => loop {
                        Self::run(interpreter.clone(), (*cmd).clone()).await;
                    },
                }
            })
        }

        pub fn stop(&self) {
            let mut opt_abort_handle = self.abort_handle.write().unwrap();
            if let Some(ref abort_handle) = *opt_abort_handle {
                info!("Terminating current blinking task");
                abort_handle.abort();
                *opt_abort_handle = None;
            }
        }

        pub async fn run_async(&self, spec: Cmd) {
            info!("Blinker run_async()");
            if let Some(ref abort_handle) = *(self.abort_handle.write().unwrap()) {
                info!("Terminating current blinking task");
                abort_handle.abort();
            }
            let interpreter = self.interpreter.clone();

            let (f, handle) =
                futures::future::abortable(async move { Self::run(interpreter, spec).await });

            info!("run_async: Spawning future");
            let _join_handle = tokio::spawn(f);
            info!("Created new blinking task");
            // let _ = _join_handle.is_ready();

            tokio::time::delay_for(std::time::Duration::from_secs(0)).await; // FIXME: why is this necessary??
            *(self.abort_handle.write().unwrap()) = Some(handle);
        }
    }
}
