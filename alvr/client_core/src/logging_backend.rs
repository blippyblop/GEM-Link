use alvr_common::{
    DebugGroupsConfig, LogSeverity,
    log::{Level, Record},
    parking_lot::Mutex,
};
use alvr_packets::ClientControlPacket;
use std::{
    sync::{LazyLock, mpsc},
    time::{Duration, Instant},
};

const LOG_REPEAT_TIMEOUT: Duration = Duration::from_secs(1);

/// The client's log filter when `RUST_LOG` says nothing.
///
/// `warn`, not `error`: warnings are how this client reports that it is holding frames, discarding
/// datagrams, failing to repair a frame, or rebuilding a stalled pipeline. A default that hides
/// all of that makes a working client and a broken one look identical from the outside.
pub const CLIENT_DEFAULT_LOG_FILTER: &str = "warn";

pub struct LogMirrorData {
    pub sender: mpsc::Sender<ClientControlPacket>,
    pub filter_level: LogSeverity,
    pub debug_groups_config: DebugGroupsConfig,
}

pub static LOG_CHANNEL_SENDER: Mutex<Option<LogMirrorData>> = Mutex::new(None);

struct RepeatedLogEvent {
    message: String,
    repetition_times: usize,
    initial_timestamp: Instant,
}

static LAST_LOG_EVENT: LazyLock<Mutex<RepeatedLogEvent>> = LazyLock::new(|| {
    Mutex::new(RepeatedLogEvent {
        message: "".into(),
        repetition_times: 0,
        initial_timestamp: Instant::now(),
    })
});

pub fn init_logging() {
    fn send_log(record: &Record) -> bool {
        let Some(data) = &*LOG_CHANNEL_SENDER.lock() else {
            // if channel has not been setup, always print everything to stdout
            // todo: the client debug groups settings should be moved client side when feasible
            return true;
        };

        let level = match record.level() {
            Level::Error => LogSeverity::Error,
            Level::Warn => LogSeverity::Warning,
            Level::Info => LogSeverity::Info,
            Level::Debug | Level::Trace => LogSeverity::Debug,
        };

        // A warning or an error describes something that is *wrong*. Verbosity settings choose how
        // much success is reported; they do not get to choose whether failure is reported, and
        // neither does the group filter — which is matched against the message text, so it can
        // swallow anything.
        //
        // This is not hypothetical. Three defects this project chased were invisible because
        // something was configured, or defaulted, to suppress the line that described them: the
        // client's `warn!`s were dropped by an Error-only default logger, the driver's `dbg_*`
        // path was dropped by a group filter that ignores level, and `avoid_video_glitching`
        // disabled the recovery itself. A diagnostic that a setting can silence is a diagnostic
        // that is missing exactly when it is needed. See ADR-0014.
        let describes_a_fault = matches!(level, LogSeverity::Error | LogSeverity::Warning);

        if !describes_a_fault {
            if level < data.filter_level {
                return false;
            }
            let message = format!("{}", record.args());
            if !alvr_common::filter_debug_groups(&message, &data.debug_groups_config) {
                return false;
            }
        }

        let message = format!("{}", record.args());

        let mut last_log_event_lock = LAST_LOG_EVENT.lock();

        if last_log_event_lock.message == message
            && last_log_event_lock.initial_timestamp + LOG_REPEAT_TIMEOUT > Instant::now()
        {
            last_log_event_lock.repetition_times += 1;
        } else {
            if last_log_event_lock.repetition_times > 1 {
                data.sender
                    .send(ClientControlPacket::Log {
                        level: LogSeverity::Info,
                        message: format!(
                            "Last log line repeated {} times",
                            last_log_event_lock.repetition_times
                        ),
                    })
                    .ok();
            }

            *last_log_event_lock = RepeatedLogEvent {
                message: message.clone(),
                repetition_times: 1,
                initial_timestamp: Instant::now(),
            };

            data.sender
                .send(ClientControlPacket::Log { level, message })
                .ok();
        }

        true
    }

    #[cfg(target_os = "android")]
    {
        android_logger::init_once(
            android_logger::Config::default()
                .with_tag("[ALVR NATIVE-RUST]")
                .format(|f, record| {
                    if send_log(record) {
                        writeln!(f, "{}", record.args())
                    } else {
                        Ok(())
                    }
                })
                .with_max_level(alvr_common::log::LevelFilter::Info),
        );
    }
    #[cfg(not(target_os = "android"))]
    {
        use std::io::Write;
        // `env_logger::builder()` with no `RUST_LOG` in the environment is **Error only**, which
        // silently discards every `warn!` the client emits. That default is how a run in this
        // project produced `0 x Network dropped video packet` — a measurement that was not
        // "never happened" but "never printed". The default is now explicitly `warn`, so warnings
        // appear without anyone having to know to ask for them, and `RUST_LOG` still overrides.
        env_logger::Builder::from_env(
            env_logger::Env::default().default_filter_or(CLIENT_DEFAULT_LOG_FILTER),
        )
        .format(|f, record| {
            if send_log(record) {
                writeln!(f, "{}", record.args())
            } else {
                Ok(())
            }
        })
        .try_init()
        .ok();
    }

    alvr_common::set_panic_hook();
}
