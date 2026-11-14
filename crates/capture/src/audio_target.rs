//! Which audio the game track records: the game process alone, or everything the speakers play.
//!
//! Per-application capture (WASAPI process loopback) keeps Discord, music and notifications out
//! of the clip. It needs a running game to target, so with none found, or where the OS cannot do
//! it, the whole default output is captured instead, and the log says which one is in use.

use crate::audio_recovery::SourceOpener;
use crate::audio_timeline::PacketSource;
use anyhow::Result;
use serde::Deserialize;

/// `[capture] game_audio`: what the game-audio track records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GameAudioSource {
    /// Only the running watched game and its child processes; the whole output when none is running.
    #[default]
    App,
    /// Everything the default output device plays.
    System,
}

/// Finds the process id of the game to capture, or `None` when no watched game is running.
pub type GameFinder = Box<dyn FnMut() -> Option<u32> + Send>;

/// Opens the two kinds of loopback stream; the WASAPI implementation, or a fake in tests.
pub(crate) trait LoopbackFactory {
    type Stream: PacketSource;
    fn open_process(&mut self, pid: u32) -> Result<Self::Stream>;
    /// The default output, with its key (see [`SourceOpener::wanted`]).
    fn open_system(&mut self) -> Result<(Self::Stream, String)>;
    fn system_key(&mut self) -> Result<String>;
}

/// Opens the game's own audio when there is a game to target, and the whole output otherwise.
pub(crate) struct GameAudioOpener<F> {
    factory: F,
    find_game: Option<GameFinder>,
    /// A process whose activation failed, so it is not retried every check while it runs.
    refused_pid: Option<u32>,
    /// Whether an attempt has already been announced. The first one of a session is a line the
    /// user reads; the retries that follow a lost device are debug lines, because they arrive
    /// twice a second for as long as the device stays away.
    announced: bool,
}

impl<F: LoopbackFactory> GameAudioOpener<F> {
    /// `find_game` is `None` for whole-output capture.
    pub fn new(factory: F, find_game: Option<GameFinder>) -> Self {
        Self {
            factory,
            find_game,
            refused_pid: None,
            announced: false,
        }
    }

    fn target(&mut self) -> Option<u32> {
        let pid = (self.find_game.as_mut()?)()?;
        (Some(pid) != self.refused_pid).then_some(pid)
    }

    /// Say what is about to be opened, before it is opened.
    ///
    /// The line is the point of the exercise: an open that hangs or fails must be visible in
    /// the log *while* it is being waited for, because a backend that never comes up is
    /// otherwise a silence with nothing to explain it. `first_attempt` separates the one
    /// attempt a user reads from the retries a lost device produces.
    fn announce(&self, what: &str, first_attempt: bool) {
        if first_attempt {
            tracing::info!("game audio: opening the audio source: {what}");
        } else {
            tracing::debug!(source = what, "game audio: opening the audio source");
        }
    }
}

fn process_key(pid: u32) -> String {
    format!("process:{pid}")
}

impl<F: LoopbackFactory> SourceOpener for GameAudioOpener<F> {
    type Stream = F::Stream;

    fn open(&mut self) -> Result<(F::Stream, String)> {
        // The first attempt of a session is the one a user reads; the retries a lost device
        // produces are debug lines. Which one this is is decided before the first attempt,
        // because the attempt itself is what may never return.
        let first_attempt = !self.announced;
        self.announced = true;
        if let Some(pid) = self.target() {
            self.announce(
                &format!("the game process {pid} (process loopback)"),
                first_attempt,
            );
            match self.factory.open_process(pid) {
                Ok(stream) => {
                    tracing::info!(
                        pid,
                        "game audio: capturing the game process only (process loopback)"
                    );
                    return Ok((stream, process_key(pid)));
                }
                Err(e) => {
                    tracing::warn!(pid, error = %format!("{e:#}"), "game audio: per-application capture failed; capturing the whole output instead");
                    self.refused_pid = Some(pid);
                }
            }
        }
        self.announce(
            "everything the default output plays (endpoint loopback)",
            first_attempt,
        );
        let (stream, key) = self.factory.open_system()?;
        tracing::info!(
            per_app = self.find_game.is_some(),
            "game audio: capturing everything the default output plays (endpoint loopback)"
        );
        Ok((stream, format!("system:{key}")))
    }

    fn wanted(&mut self) -> Result<String> {
        match self.target() {
            Some(pid) => Ok(process_key(pid)),
            None => Ok(format!("system:{}", self.factory.system_key()?)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_recovery::RecoveringSource;
    use crate::audio_timeline::{AudioTimeline, PacketInfo};
    use crate::AudioFormat;
    use anyhow::anyhow;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// A stream that says which source it is through the value of every sample.
    struct Tagged(u8, bool);

    impl PacketSource for Tagged {
        fn pull(&mut self, out: &mut Vec<u8>) -> Result<Option<PacketInfo>> {
            if std::mem::take(&mut self.1) {
                out.extend(std::iter::repeat_n(self.0, 480 * 4));
                return Ok(Some(PacketInfo {
                    frames: 480,
                    time: None,
                    discontinuity: false,
                }));
            }
            Ok(None)
        }
    }

    #[derive(Default)]
    struct Fake {
        refuse_process: bool,
        opened: Vec<String>,
        /// The log as it stood when the factory was asked to open: the proof that the
        /// attempt was announced before it was made.
        log_at_open: Vec<String>,
        logs: Log,
    }

    impl LoopbackFactory for Fake {
        type Stream = Tagged;
        fn open_process(&mut self, pid: u32) -> Result<Tagged> {
            if self.refuse_process {
                return Err(anyhow!("activation refused"));
            }
            self.log_at_open = self.logs.lines();
            self.opened.push(process_key(pid));
            Ok(Tagged(2, true))
        }
        fn open_system(&mut self) -> Result<(Tagged, String)> {
            self.log_at_open = self.logs.lines();
            self.opened.push("system".into());
            Ok((Tagged(1, true), "speakers".into()))
        }
        fn system_key(&mut self) -> Result<String> {
            Ok("speakers".into())
        }
    }

    /// The log lines the events carried, in order.
    ///
    /// A subscriber of its own, because the claim under test is about *when* a line is
    /// written: the fake factory records the log as it was at the moment it was asked to
    /// open, so a line written only after the open succeeded would prove nothing.
    ///
    /// Only the levels a user reads are recorded: the debug lines are what a retry says,
    /// and the point of one test is that a retry is not one of them.
    #[derive(Clone, Default)]
    struct Log(Arc<Mutex<Vec<String>>>);

    impl Log {
        fn lines(&self) -> Vec<String> {
            self.0.lock().unwrap_or_else(|p| p.into_inner()).clone()
        }
    }

    impl tracing::Subscriber for Log {
        fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
            *metadata.level() <= tracing::Level::INFO
        }

        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            let mut message = String::new();
            event.record(&mut Message(&mut message));
            self.0
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(message);
        }

        fn enter(&self, _span: &tracing::span::Id) {}

        fn exit(&self, _span: &tracing::span::Id) {}
    }

    thread_local! {
        /// The [`Log`] this thread's events go to while a test is capturing them.
        static CAPTURING: std::cell::RefCell<Option<Log>> = const { std::cell::RefCell::new(None) };
    }

    /// Run `f` with the events this thread emits recorded in `logs`.
    ///
    /// Not `tracing::subscriber::with_default`. A callsite's interest is cached for the whole
    /// process when the callsite is first hit, and the other tests here hit `announce` on their
    /// own threads with no subscriber at all. The cache could then read "never" just as a
    /// capturing test began, and that test's subscriber was never asked: 12 runs in 400 with the
    /// tests in parallel, none alone or one thread at a time. One global subscriber that is always
    /// interested and routes events by thread has no such window.
    fn capture<R>(logs: &Log, f: impl FnOnce() -> R) -> R {
        static INSTALL: std::sync::Once = std::sync::Once::new();
        INSTALL.call_once(|| {
            tracing::subscriber::set_global_default(Router)
                .expect("nothing else in this test binary installs a global subscriber");
        });
        CAPTURING.with(|current| *current.borrow_mut() = Some(logs.clone()));
        let out = f();
        CAPTURING.with(|current| *current.borrow_mut() = None);
        out
    }

    /// The global subscriber behind [`capture`]: interested in every callsite, and handing each
    /// event to the [`Log`] of the thread that emitted it, if that thread is capturing.
    struct Router;

    impl tracing::Subscriber for Router {
        fn register_callsite(
            &self,
            _metadata: &'static tracing::Metadata<'static>,
        ) -> tracing::subscriber::Interest {
            tracing::subscriber::Interest::sometimes()
        }

        fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
            CAPTURING.with(|current| {
                current
                    .borrow()
                    .as_ref()
                    .is_some_and(|log| tracing::Subscriber::enabled(log, metadata))
            })
        }

        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            CAPTURING.with(|current| {
                if let Some(log) = current.borrow().as_ref() {
                    tracing::Subscriber::event(log, event);
                }
            });
        }

        fn enter(&self, _span: &tracing::span::Id) {}

        fn exit(&self, _span: &tracing::span::Id) {}
    }

    /// Takes the event's `message`, which is where the text of a `tracing::info!` lives.
    struct Message<'a>(&'a mut String);

    impl tracing::field::Visit for Message<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.0.push_str(&format!("{value:?}"));
            }
        }
    }

    /// A game finder whose answer the test changes.
    fn finder() -> (Arc<Mutex<Option<u32>>>, GameFinder) {
        let pid = Arc::new(Mutex::new(None));
        let read = pid.clone();
        (pid, Box::new(move || *read.lock().unwrap()))
    }

    #[test]
    fn the_game_is_captured_alone_once_it_runs_and_the_output_again_after_it_exits() {
        let (pid, find) = finder();
        let opener = GameAudioOpener::new(Fake::default(), Some(find));
        let format = AudioFormat {
            sample_rate: 48_000,
            channels: 2,
        };
        let ms = Duration::from_millis;
        let mut source =
            RecoveringSource::start(opener, AudioTimeline::new(format, ms(0)), ms(0)).unwrap();

        *pid.lock().unwrap() = Some(42);
        source.poll(ms(1_000)).unwrap();
        *pid.lock().unwrap() = None;
        source.poll(ms(2_000)).unwrap();
        source.poll(ms(2_010)).unwrap();

        let opened = &source_factory(&source).opened;
        assert_eq!(opened, &["system", "process:42", "system"]);
        assert_eq!(source.generation(), 3);
    }

    fn source_factory(source: &RecoveringSource<GameAudioOpener<Fake>>) -> &Fake {
        &source.opener().factory
    }

    #[test]
    fn a_refused_activation_falls_back_to_the_output_and_is_not_retried() {
        let (pid, find) = finder();
        *pid.lock().unwrap() = Some(7);
        let mut opener = GameAudioOpener::new(
            Fake {
                refuse_process: true,
                ..Fake::default()
            },
            Some(find),
        );
        let (_, key) = opener.open().unwrap();
        assert_eq!(key, "system:speakers");
        assert_eq!(
            opener.wanted().unwrap(),
            key,
            "so the open stream is kept, not reopened every second"
        );

        // A new game process is tried again.
        *pid.lock().unwrap() = Some(8);
        assert_eq!(opener.wanted().unwrap(), "process:8");
    }

    #[test]
    fn whole_output_mode_never_looks_for_the_game() {
        let mut opener = GameAudioOpener::new(Fake::default(), None);
        assert_eq!(opener.open().unwrap().1, "system:speakers");
        assert_eq!(opener.wanted().unwrap(), "system:speakers");
    }

    /// The attempt is announced before it is made.
    ///
    /// The claim is about the order, not the wording: a line written after the open returned
    /// says nothing about an open that never returns, which is the failure this guards. A
    /// backend that hangs or fails is invisible in the log without it.
    #[test]
    fn the_source_is_announced_before_the_open_is_attempted() {
        let logs = Log::default();
        let mut opener = GameAudioOpener::new(
            Fake {
                logs: logs.clone(),
                ..Fake::default()
            },
            None,
        );
        let opened = capture(&logs, || opener.open().expect("the fake opens"));
        assert_eq!(opened.1, "system:speakers");

        let at_open = &opener.factory.log_at_open;
        assert!(
            at_open
                .iter()
                .any(|line| line.contains("opening the audio source")
                    && line.contains("default output")),
            "the attempt must be announced, and named, before it is made: the log when the open \
             was attempted was {at_open:?}"
        );
    }

    /// The retry that follows a lost device is not announced again at the level a user reads.
    ///
    /// The device comes and goes twice a second while it is away, and the warning that
    /// reports the outage is the line to read; an announcement per retry would bury it.
    #[test]
    fn a_retry_after_a_lost_device_is_not_announced_again() {
        let logs = Log::default();
        let mut opener = GameAudioOpener::new(
            Fake {
                logs: logs.clone(),
                ..Fake::default()
            },
            None,
        );
        capture(&logs, || {
            opener.open().expect("the fake opens");
            opener.open().expect("the fake opens again");
        });
        let announced = logs
            .lines()
            .into_iter()
            .filter(|line| line.contains("opening the audio source"))
            .count();
        assert_eq!(
            announced, 1,
            "the second attempt is a retry, and the log says so once"
        );
    }

    #[test]
    fn the_setting_reads_as_lowercase_words() {
        #[derive(Deserialize)]
        struct Section {
            game_audio: GameAudioSource,
        }
        let parse = |v: &str| {
            serde_json::from_str::<Section>(&format!("{{\"game_audio\":\"{v}\"}}"))
                .map(|s| s.game_audio)
        };
        assert_eq!(parse("app").unwrap(), GameAudioSource::App);
        assert_eq!(parse("system").unwrap(), GameAudioSource::System);
        assert!(parse("discord").is_err());
    }
}
