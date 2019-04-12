extern crate chrono;
extern crate r2d2;
extern crate r2d2_redis;
extern crate redis;
extern crate serde_json;
extern crate slog;

use chrono::{DateTime, SecondsFormat, Utc};
use r2d2_redis::RedisConnectionManager;
use serde_json::json;
use slog::{OwnedKVList, Record, KV};
use std::fmt;
use std::process::Command;
use std::time::Duration;

/// A logger that sends JSON formatted logs to a list in a Redis instance. It uses this format
///
///   {
///     "@timestamp": ${timeRFC3339},
///     "@source_host": ${hostname},
///     "@message": ${message},
///     "@fields": {
///        "level": ${levelLowercase},
///        "application": ${appName}
///    }
///
/// It supports structured logging via [`slog`][slog-url],
///
/// This struct implements the `Log` trait from the [`log` crate][log-crate-url],
/// which allows it to act as a logger.
///
/// You can use the [`Builder`] to construct it and then install it with
/// the [`log` crate][log-crate-url] directly.
///
/// [log-crate-url]: https://docs.rs/log/
/// [`Builder`]: struct.Builder.html
/// [slog-url]: https://github.com/slog-rs/slog
#[derive(Debug)]
pub struct Logger {
    config: LoggerConfig,
    pool: r2d2::Pool<RedisConnectionManager>,
}

/// Builds the Redis logger.
#[derive(Default, Debug)]
pub struct Builder {
    redis_host: String,
    redis_port: u32,
    redis_key: String,
    app_name: String,
    hostname: Option<String>,
    ttl_seconds: Option<u64>,
}

#[derive(Debug)]
pub enum Error {
    ConnectionPoolErr(r2d2::Error),
    RedisErr(redis::RedisError),
    LogErr(slog::Error),
}

#[derive(Debug)]
struct LoggerConfig {
    pub redis_host: String,
    pub redis_port: u32,
    pub redis_key: String,
    pub app_name: String,
    pub hostname: String,
    pub ttl_seconds: Option<u64>,
}

#[allow(dead_code)]
impl Builder {
    pub fn new(app_name: &str) -> Builder {
        Builder {
            app_name: app_name.to_string(),
            redis_host: "localhost".to_string(),
            redis_port: 6379,
            ..Default::default()
        }
    }

    pub fn redis(self, host: String, port: u32, key: String) -> Builder {
        Builder {
            redis_host: host,
            redis_port: port,
            redis_key: key,
            ..self
        }
    }

    pub fn redis_key(self, key: &str) -> Builder {
        Builder {
            redis_key: key.to_string(),
            ..self
        }
    }

    pub fn redis_host(self, host: &str) -> Builder {
        Builder {
            redis_host: host.to_string(),
            ..self
        }
    }

    pub fn redis_port(self, val: u32) -> Builder {
        Builder {
            redis_port: val,
            ..self
        }
    }

    pub fn ttl(self, duration: Duration) -> Builder {
        Builder {
            ttl_seconds: Some(duration.as_secs()),
            ..self
        }
    }

    /// Builds the redis logger
    ///
    /// The returned logger implements the `Log` trait and can be installed manually
    /// or nested within another logger.
    pub fn build(self) -> Result<Logger, Error> {
        // TODO: Get something that works on windows too
        fn get_host_name() -> String {
            let output = Command::new("hostname").output().expect("failed to execute process");
            String::from_utf8_lossy(&output.stdout).replace("\n", "").to_string()
        }

        let con_str = format!("redis://{}:{}", self.redis_host, self.redis_port);
        let manager = RedisConnectionManager::new(con_str.as_str())?;
        let pool = r2d2::Pool::builder()
            .connection_timeout(Duration::new(1, 0))
            .build(manager)?;

        let con = pool.get()?;
        let _: () = redis::cmd("PING").query(&*con)?;

        Ok(Logger {
            config: LoggerConfig {
                redis_host: self.redis_host,
                redis_port: self.redis_port,
                redis_key: self.redis_key,
                app_name: self.app_name,
                hostname: self.hostname.unwrap_or_else(get_host_name),
                ttl_seconds: self.ttl_seconds,
            },
            pool,
        })
    }
}

type KeyVals = std::vec::Vec<(String, serde_json::Value)>;

impl Logger {
    fn v0_msg(&self, level: String, msg: String, key_vals: Option<KeyVals>) -> String {
        let now: DateTime<Utc> = Utc::now();
        let time = now.to_rfc3339_opts(SecondsFormat::AutoSi, true);
        let application = self.config.app_name.clone();
        let mut json_val = json!({
            "@timestamp": time,
            "@source_host": self.config.hostname.clone(),
            "@message": msg,
            "@fields": {
                "level": level,
                "application": application
            }
        });

        let fields = match json_val {
            serde_json::Value::Object(ref mut v) => match v.get_mut("@fields").unwrap() {
                serde_json::Value::Object(ref mut v) => Some(v),
                _ => None,
            },
            _ => None,
        }
            .unwrap();

        for key_val in &key_vals.unwrap() {
            fields.insert(key_val.0.clone(), key_val.1.clone());
        }

        json_val.to_string()
    }

    /// Sends a message constructed by v0_msg to the redis server
    fn send_to_redis(&self, msg: &String) -> Result<(), Error> {
        let con = self.pool.get()?;

        redis::cmd("RPUSH")
            .arg(self.config.redis_key.clone())
            .arg(msg)
            .query(&*con)?;

        if let Some(t) = self.config.ttl_seconds {
            redis::cmd("EXPIRE")
                .arg(self.config.redis_key.clone())
                .arg(t)
                .query(&*con)?
        }
        Ok(())
    }
}

impl log::Log for Logger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Info
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            let msg = self.v0_msg(record.level().to_string(), format!("{}", record.args()), None);
            let mut prefix = String::from("");
            if let Err(e) = self.send_to_redis(&msg) {
                prefix = format!("fallback logger: [{}]", e);
            }
            println!("{}{}", prefix, msg);
        }
    }

    fn flush(&self) {}
}

impl From<r2d2::Error> for Error {
    fn from(error: r2d2::Error) -> Self {
        Error::ConnectionPoolErr(error)
    }
}

impl From<redis::RedisError> for Error {
    fn from(error: redis::RedisError) -> Self {
        Error::RedisErr(error)
    }
}

impl From<slog::Error> for Error {
    fn from(error: slog::Error) -> Self {
        Error::LogErr(error)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::ConnectionPoolErr(_e) => write!(f, "Redis logger connection pool error"),
            Error::RedisErr(_e) => write!(f, "Redis logger Redis error"),
            Error::LogErr(_e) => write!(f, "redis logger slog error"),
        }
    }
}

impl slog::Drain for Logger {
    type Ok = ();
    type Err = self::Error;

    fn log(&self, record: &Record, values: &OwnedKVList) -> Result<Self::Ok, Self::Err> {
        let level_str = record.level().to_string();
        let ser = &mut Serializer::new();
        record.kv().serialize(record, ser)?;
        values.serialize(record, ser)?;

        //let keyVals = vec![("Hallo".to_string(), serde_json::Value::String("world!".to_string()))];
        let msg = self.v0_msg(level_str, format!("{}", record.msg()), Some(ser.done()));
        self.send_to_redis(&msg)?;
        Ok(())
    }
}

struct Serializer {
    vec: KeyVals,
}

impl Serializer {
    pub fn new() -> Serializer {
        Serializer { vec: Vec::new() }
    }

    pub fn emit_val(&mut self, key: slog::Key, val: serde_json::Value) -> slog::Result {
        self.vec.push((key.to_string(), val));
        Ok(())
    }

    fn emit_serde_json_number<V>(&mut self, key: Key, value: V) -> slog::Result
        where
            serde_json::Number: From<V>,
    {
        // convert a given number into serde_json::Number
        let num = serde_json::Number::from(value);
        self.emit_val(key, serde_json::Value::Number(num))
    }

    fn done(&mut self) -> KeyVals {
        self.vec.clone()
    }
}

use core::fmt::Write;
use slog::Key;

#[allow(dead_code)]
impl slog::Serializer for Serializer {
    // TODO: Implement other methods

    fn emit_bool(&mut self, key: Key, val: bool) -> slog::Result {
        self.emit_val(key, serde_json::Value::Bool(val))
    }

    fn emit_unit(&mut self, key: Key) -> slog::Result {
        self.emit_val(key, serde_json::Value::Null)
    }

    fn emit_str(&mut self, key: Key, val: &str) -> slog::Result {
        self.emit_val(key, serde_json::Value::String(val.to_string()))
    }

    fn emit_arguments(&mut self, key: Key, val: &fmt::Arguments) -> slog::Result {
        let mut buf = String::with_capacity(128);
        buf.write_fmt(*val).unwrap();
        self.emit_val(key, serde_json::Value::String(buf))
    }
}
