use crate::kernel_metadata::errno_name;
use backtrace::Backtrace;
use nix::errno::errno;
use std::{
    collections::HashMap,
    env,
    env::var_os,
    fs::{File, OpenOptions},
    io::{self, BufWriter, Result, Write},
    path::Path,
    sync::{Mutex, MutexGuard},
};

#[derive(Clone)]
struct LogModule {
    name: String,
    level: LogLevel,
}

#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Debug)]
pub enum LogLevel {
    LogFatal,
    LogError,
    LogWarn,
    LogInfo,
    LogDebug,
}

use crate::session::task::Task;
pub use LogLevel::*;

struct LogGlobals {
    level_map: HashMap<String, LogLevel>,
    log_modules_cache: HashMap<String, LogModule>,
    logging_stream: String,
    /// Possibly buffered
    log_file: Box<dyn Write + Send>,
    default_level: LogLevel,
}

/// @TODO Will this work in all situations?
extern "C" fn flush_log_buffer() {
    let mut maybe_log_lock = LOG_GLOBALS.lock();
    match &mut maybe_log_lock {
        Ok(lock) => {
            lock.log_file.flush().unwrap_or(());
        }
        Err(e) => panic!(
            "Could not obtain lock on rd log. Can't flush log buffer: {:?}",
            e
        ),
    };
}

lazy_static! {
    static ref LOG_GLOBALS: Mutex<LogGlobals> = {
        let maybe_filename = var_os("RD_LOG_FILE");
        let maybe_append_filename = var_os("RD_APPEND_LOG_FILE");
        // @TODO Ok to simply add Sync + Send?
        let mut f: Box<dyn Write + Sync + Send>;
        if let Some(filename) = maybe_filename {
            f = Box::new(File::create(&filename).expect(&format!("Error. Could not create filename `{:?}' specified in environment variable RD_LOG_FILE", filename)));
        } else if let Some(append_filename) = maybe_append_filename {
            f = Box::new(OpenOptions::new().append(true).create(true).open(&append_filename).expect(&format!("Error. Could not append to filename `{:?}' specified in env variable RD_APPEND_LOG_FILE", append_filename)));
        } else {
            f = Box::new(io::stderr());
        }

        let maybe_buf_size = env::var("RD_LOG_BUFFER");
        if let Ok(buf_size) = maybe_buf_size {
            let log_buffer_size = buf_size.parse::<usize>().expect(&format!("Error. Could not parse `{:?}' in environment var `RD_LOG_BUFFER' as a number", buf_size));
            f = Box::new(BufWriter::with_capacity(log_buffer_size, f));
        }

        let ret = unsafe {
            libc::atexit(flush_log_buffer)
        };
        assert_eq!(ret, 0);

        let (default_level, level_map) = match env::var("RD_LOG") {
            Ok(rd_log) => init_log_levels(&rd_log),
            Err(_) => (LogWarn, HashMap::new())
        };

        Mutex::new(LogGlobals {
            level_map,
            log_modules_cache: HashMap::new(),
            logging_stream: String::new(),
            // Possibly buffered
            log_file: f,
            default_level,
        })
    };
}

fn log_level_string_to_level(log_level_string: &str) -> LogLevel {
    match log_level_string {
        "fatal" => LogFatal,
        "error" => LogError,
        "warn" => LogWarn,
        "info" => LogInfo,
        "debug" => LogDebug,
        _ => LogWarn,
    }
}

fn init_log_levels(rd_log: &str) -> (LogLevel, HashMap<String, LogLevel>) {
    let mut hm: HashMap<String, LogLevel> = HashMap::new();
    let mod_colon_levels = rd_log.split(',');
    let mut default_level = LogWarn;
    for mod_colon_level in mod_colon_levels {
        let res: Vec<&str> = mod_colon_level.splitn(2, ':').collect();
        if res.len() == 2 {
            let mod_name = res[0].trim();
            let log_level_string = res[1].trim();
            if mod_name == "all" {
                default_level = log_level_string_to_level(log_level_string);
            } else {
                hm.insert(
                    mod_name.to_owned(),
                    log_level_string_to_level(log_level_string),
                );
            }
        }
    }
    (default_level, hm)
}

/// Given a module name, what is its log level?
fn get_log_level(module_name: &str, l: &MutexGuard<LogGlobals>) -> LogLevel {
    // We DONT lowercase here as filenames are usually case sensitive on Linux.
    let maybe_log_level = l.level_map.get(module_name);
    if let Some(log_level) = maybe_log_level {
        *log_level
    } else {
        l.default_level
    }
}

/// Given a filename what is the corresponding module name?
fn filename_to_module_name(filename: &str) -> String {
    let path = Path::new(filename);
    // Note: DONT lowercase this.
    path.file_stem().unwrap().to_string_lossy().to_string()
}

/// Given the filename get the corresponding LogModule.
fn get_log_module(filename: &str, l: &mut MutexGuard<LogGlobals>) -> LogModule {
    let maybe_log_module = l.log_modules_cache.get(filename);
    if let Some(log_module) = maybe_log_module {
        log_module.to_owned()
    } else {
        let name = filename_to_module_name(filename);
        let level = get_log_level(&name, l);
        let m = LogModule { level, name };
        l.log_modules_cache.insert(filename.to_owned(), m.clone());
        m
    }
}

fn set_all_logging(level: LogLevel, l: &mut MutexGuard<LogGlobals>) {
    l.default_level = level;
    l.level_map.clear();
    l.log_modules_cache.clear();
}

fn set_logging(module_name: &str, level: LogLevel, l: &mut MutexGuard<LogGlobals>) {
    l.level_map.insert(module_name.to_owned(), level);
    l.log_modules_cache.clear();
}

fn log_name(level: LogLevel) -> String {
    match level {
        LogFatal => "FATAL".into(),
        LogError => "ERROR".into(),
        LogWarn => "WARN".into(),
        LogInfo => "INFO".into(),
        LogDebug => "DEBUG".into(),
    }
}

pub struct NewLineTerminatingOstream {
    enabled: bool,
    level: LogLevel,
    message: Vec<u8>,
    lock: MutexGuard<'static, LogGlobals>,
}

impl NewLineTerminatingOstream {
    fn new(
        level: LogLevel,
        filename: &str,
        line: u32,
        func_name: &str,
        always_enabled: bool,
    ) -> Option<NewLineTerminatingOstream> {
        let mut lock = LOG_GLOBALS.lock().unwrap();
        let m = get_log_module(filename, &mut lock);
        let enabled = always_enabled || level <= m.level;
        if enabled {
            let mut stream = NewLineTerminatingOstream {
                message: Vec::new(),
                enabled,
                level,
                lock,
            };
            if level == LogDebug {
                write!(stream, "[{}] ", m.name).unwrap();
            } else {
                write_prefix(&mut stream, level, filename, line, func_name);
            }

            Some(stream)
        } else {
            None
        }
    }
}

/// Low level. Use is_logging!() macro instead.
pub fn is_logging(level: LogLevel, filename: &str, _line: u32, _func_name: &str) -> bool {
    let mut lock = LOG_GLOBALS.lock().unwrap();
    let m = get_log_module(filename, &mut lock);
    let enabled = level <= m.level;
    enabled
}

impl Drop for NewLineTerminatingOstream {
    fn drop(&mut self) {
        if self.enabled {
            self.write(b"\n").unwrap();
            // This flushes self.message *to* the log file
            // (which could be stderr or a log file or a buffered writer that wraps stderr
            //  or a buffered writer that wraps some log file).
            // BUT, It does NOT flush the log file itself.
            self.flush().unwrap_or(());
        }
    }
}

impl Write for NewLineTerminatingOstream {
    /// Write the text stored in the `message` member to the log file.
    fn flush(&mut self) -> Result<()> {
        if self.message.len() > 0 && self.enabled {
            self.lock.log_file.write_all(&self.message)?;
            // We DONT flush the log file. This is handled automatically.
        }
        self.message.clear();
        Ok(())
    }

    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        if self.enabled {
            self.message.extend_from_slice(buf);
        }

        // Need to pretend these were written even if buffer was not enabled.
        // Otherwise we get a `Err` value
        // Custom { kind: WriteZero, error: "failed to write whole buffer" }
        Ok(buf.len())
    }
}

pub fn write_prefix(
    stream: &mut dyn Write,
    level: LogLevel,
    filename: &str,
    line: u32,
    func_name: &str,
) {
    write!(stream, "[{} ", log_name(level)).unwrap();
    if level <= LogError {
        write!(stream, "{}:{} ", filename, line).unwrap();
    }

    write!(stream, "{}()", func_name).unwrap();
    let err = errno();
    if level <= LogWarn && err != 0 {
        write!(stream, " errno: {}", errno_name(err)).unwrap();
    }
    write!(stream, "] ").unwrap();
}

pub fn log(
    log_level: LogLevel,
    filename: &str,
    line: u32,
    module_path: &str,
    always_enabled: bool,
) -> Option<NewLineTerminatingOstream> {
    NewLineTerminatingOstream::new(log_level, filename, line, module_path, always_enabled)
}

/// Outputs to (possibly write buffered) log file (or stderr if no log file was specified)
/// After this program continues normally.
macro_rules! log {
    ($log_level:expr, $($args:tt)+) => {
        {
            use std::io::Write;
            let maybe_stream = crate::log::log(
                $log_level,
                file!(),
                line!(),
                module_path!(),
                false
            );
            match maybe_stream {
                Some(mut stream) => write!(stream, $($args)+).unwrap(),
                None => ()
            }
        }
    };
}

macro_rules! is_logging {
    ($log_level:expr) => {
        crate::log::is_logging($log_level, file!(), line!(), module_path!())
    };
}

/// Outputs to (possibly write buffered) log file (or stderr if no log file was specified)
/// Prints out the backtrace to stderr and aborts.
macro_rules! fatal {
    ($($args:tt)+) => {
        {
            {
                use std::io::Write;
                use crate::log::LogFatal;
                let maybe_stream = crate::log::log(
                    LogFatal,
                    file!(),
                    line!(),
                    module_path!(),
                    true
                );
                match maybe_stream {
                   Some(mut stream) => write!(stream, $($args)+).unwrap(),
                   None => ()
                }
            }
            crate::log::notifying_abort(backtrace::Backtrace::new());
            unreachable!();
        }
    };
}

/// Output to stderr always. No backtrace -- simply exit.
macro_rules! clean_fatal {
    ($($args:tt)+) => {
        use std::io::stderr;
        crate::log::write_prefix(&mut stderr(), crate::log::LogLevel::LogFatal, file!(), line!(), module_path!());
        eprintln!($($args)+);
        std::process::exit(1);
    };
}

/// Dump the stacktrace and abort.
pub fn notifying_abort(bt: Backtrace) {
    flush_log_buffer();
    // @TODO running under test monitor stuff.
    dump_rd_stack(bt);
    std::process::abort();
}

/// Write the backtrace to stderr.
fn dump_rd_stack(bt: Backtrace) {
    eprintln!("=== Start rd backtrace:");
    eprintln!("{:?}", bt);
    eprintln!("=== End rd backtrace");
}

/// If asserting fails, start an emergency debug session
macro_rules! ed_assert {
    ($task:expr, $cond:expr) => {
        {
            use crate::session::task::task_inner::TaskInner;
            // For type checking. Will use this param later though.
            let t : &TaskInner = $task;
            if !$cond {
                {
                    use std::io::Write;
                    use crate::log::LogFatal;
                    let maybe_stream = crate::log::log(
                        LogFatal,
                        file!(),
                        line!(),
                        module_path!(),
                        true
                    );
                    match maybe_stream {
                       Some(mut stream) => {
                           write!(stream, "\n (task {} (rec: {}) at time {})\n", t.tid, t.rec_tid, t.trace_time()).unwrap();
                           write!(stream, " -> Assertion `{}' failed to hold. ", stringify!($cond)).unwrap();
                       },
                       None => ()
                    }
               }
                // @TODO this should be replaced with starting an emergency debug session
                crate::log::notifying_abort(backtrace::Backtrace::new());
            }
        }
    };
    ($task:expr, $cond:expr, $($args:tt)+) => {
        {
            use crate::session::task::task_inner::TaskInner;
            // For type checking. Will use this param later though.
            let t : &TaskInner = $task;
            if !$cond {
                {
                    use std::io::Write;
                    use crate::log::LogFatal;
                    let maybe_stream = crate::log::log(
                        LogFatal,
                        file!(),
                        line!(),
                        module_path!(),
                        true
                    );
                    match maybe_stream {
                       Some(mut stream) => {
                           write!(stream, "\n (task {} (rec: {}) at time {})\n", t.tid, t.rec_tid, t.trace_time()).unwrap();
                           write!(stream, " -> Assertion `{}' failed to hold. ", stringify!($cond)).unwrap();
                           write!(stream, $($args)+).unwrap();
                       },
                       None => ()
                    }
               }
                // @TODO this should be replaced with starting an emergency debug session
                crate::log::notifying_abort(backtrace::Backtrace::new());
            }
        }
    };
}

/// If asserting fails, start an emergency debug session
macro_rules! ed_assert_eq {
    ($task:expr, $cond1:expr, $cond2:expr) => {
        {
            use crate::session::task::task_inner::TaskInner;
            // For type checking. Will use this param later though.
            let t : &TaskInner = $task;
            let val1 = $cond1;
            let val2 = $cond2;
            if val1 != val2 {
                {
                    use std::io::Write;
                    use crate::log::LogFatal;
                    let maybe_stream = crate::log::log(
                        LogFatal,
                        file!(),
                        line!(),
                        module_path!(),
                        true
                    );
                    match maybe_stream {
                       Some(mut stream) => {
                           write!(stream, "\n (task {} (rec: {}) at time {})\n", t.tid, t.rec_tid, t.trace_time()).unwrap();
                           write!(
                               stream, " -> Assertion `{} == {}` failed to hold.\n    Left: `{:?}`, Right: `{:?}`\n",
                               stringify!($cond1), stringify!($cond2), val1, val2).unwrap();
                        },
                       None => ()
                    }
               }
                // @TODO this should be replaced with starting an emergency debug session
                crate::log::notifying_abort(backtrace::Backtrace::new());
            }
        }
    };
    ($task:expr, $cond1:expr, $cond2:expr, $($args:tt)+) => {
        {
            use crate::session::task::task_inner::TaskInner;
            // For type checking. Will use this param later though.
            let t : &TaskInner = $task;
            let val1 = $cond1;
            let val2 = $cond2;
            if val1 != val2 {
                {
                    use std::io::Write;
                    use crate::log::LogFatal;
                    let maybe_stream = crate::log::log(
                        LogFatal,
                        file!(),
                        line!(),
                        module_path!(),
                        true
                    );
                    match maybe_stream {
                       Some(mut stream) => {
                           write!(stream, "\n (task {} (rec: {}) at time {})\n", t.tid, t.rec_tid, t.trace_time()).unwrap();
                           write!(
                               stream, " -> Assertion `{} == {}` failed to hold.\n    Left: `{:?}`, Right: `{:?}`\n",
                               stringify!($cond1), stringify!($cond2), val1, val2).unwrap();
                           write!(stream, $($args)+).unwrap();
                       },
                       None => ()
                    }
               }
                // @TODO this should be replaced with starting an emergency debug session
                crate::log::notifying_abort(backtrace::Backtrace::new());
            }
        }
    };
}

/// If asserting fails, start an emergency debug session
macro_rules! ed_assert_ne {
    ($task:expr, $cond1:expr, $cond2:expr) => {
        {
            use crate::session::task::task_inner::TaskInner;
            // For type checking. Will use this param later though.
            let t : &TaskInner = $task;
            let val1 = $cond1;
            let val2 = $cond2;
            if val1 == val2 {
                {
                    use std::io::Write;
                    use crate::log::LogFatal;
                    let maybe_stream = crate::log::log(
                        LogFatal,
                        file!(),
                        line!(),
                        module_path!(),
                        true
                    );
                    match maybe_stream {
                       Some(mut stream) => {
                           write!(stream, "\n (task {} (rec: {}) at time {})\n", t.tid, t.rec_tid, t.trace_time()).unwrap();
                           write!(
                               stream, " -> Assertion `{} != {}` failed to hold.\n    Left: `{:?}`, Right: `{:?}`\n",
                               stringify!($cond1), stringify!($cond2), val1, val2).unwrap();
                        },
                       None => ()
                    }
               }
                // @TODO this should be replaced with starting an emergency debug session
                crate::log::notifying_abort(backtrace::Backtrace::new());
            }
        }
    };
    ($task:expr, $cond1:expr, $cond2:expr, $($args:tt)+) => {
        {
            use crate::session::task::task_inner::TaskInner;
            // For type checking. Will use this param later though.
            let t : &TaskInner = $task;
            let val1 = $cond1;
            let val2 = $cond2;
            if val1 == val2 {
                {
                    use std::io::Write;
                    use crate::log::LogFatal;
                    let maybe_stream = crate::log::log(
                        LogFatal,
                        file!(),
                        line!(),
                        module_path!(),
                        true
                    );
                    match maybe_stream {
                       Some(mut stream) => {
                           write!(stream, "\n (task {} (rec: {}) at time {})\n", t.tid, t.rec_tid, t.trace_time()).unwrap();
                           write!(
                               stream, " -> Assertion `{} != {}` failed to hold.\n    Left: `{:?}`, Right: `{:?}`\n",
                               stringify!($cond1), stringify!($cond2), val1, val2).unwrap();
                           write!(stream, $($args)+).unwrap();
                       },
                       None => ()
                    }
               }
                // @TODO this should be replaced with starting an emergency debug session
                crate::log::notifying_abort(backtrace::Backtrace::new());
            }
        }
    };
}

fn emergency_debug(_t: &dyn Task) {
    unimplemented!()
}
