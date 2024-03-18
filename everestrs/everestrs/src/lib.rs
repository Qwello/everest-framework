use everestrs_build::schema;

use argh::FromArgs;
use log::debug;
use serde::de::DeserializeOwned;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::Weak;
use thiserror::Error;

// Reexport everything so the clients can use it.
pub use serde;
pub use serde_json;

#[derive(Error, Debug)]
pub enum Error {
    #[error("missing argument to command call: '{0}'")]
    MissingArgument(&'static str),
    #[error("invalid argument to command call: '{0}'")]
    InvalidArgument(&'static str),
    #[error("Mismatched type: Variant contains '{0}'")]
    MismatchedType(String),
}

pub type Result<T> = ::std::result::Result<T, Error>;

#[cxx::bridge]
mod ffi {
    extern "Rust" {
        type Runtime;
        fn handle_command(
            self: &Runtime,
            implementation_id: &str,
            name: &str,
            json: JsonBlob,
        ) -> JsonBlob;
        fn handle_variable(self: &Runtime, implementation_id: &str, name: &str, json: JsonBlob);
        fn on_ready(&self);
    }

    struct JsonBlob {
        data: Vec<u8>,
    }

    /// The possible types a config can have. Note: Naturally this would be am
    /// enum **with** values - however, cxx can't (for now) map Rusts enums to
    /// std::variant or union.
    #[derive(Debug)]
    enum ConfigType {
        Boolean = 0,
        String = 1,
        Number = 2,
        Integer = 3,
    }

    /// One config entry: As said above, we can't use an enum and have to
    /// declare all values. Note: also Option is not an option...
    struct ConfigField {
        /// The name of the option, e.x. `max_voltage`.
        name: String,

        /// Our poor-mans enum.
        config_type: ConfigType,

        /// The value of the config field. The field has only a meaning if
        /// `conifg_type is set to [ConfigType::Boolean].
        bool_value: bool,

        /// The value of the config field. The field has only a meaning if
        /// `config_type` is set to [ConfigType::String].
        string_value: String,

        /// The value of the config field. The field has only a meaning if
        /// `config_type` is set to [ConfigType::Number].
        number_value: f64,

        /// The value of the config field. The field has only a meaning if
        /// `config_type` is set to [ConfigType::Integer].
        integer_value: i64,
    }

    /// The configs of one module. Roughly maps to the cpp's counterpart
    /// `ModuleConfig`.
    struct RsModuleConfig {
        /// The name of the group, e.x. "PowerMeter".
        module_name: String,

        /// All `ConfigFields` in this group.
        data: Vec<ConfigField>,
    }

    unsafe extern "C++" {
        include!("everestrs/src/everestrs_sys.hpp");

        type Module;
        fn create_module(module_id: &str, prefix: &str, conf: &str) -> UniquePtr<Module>;

        /// Connects to the message broker and launches the main everest thread to push work
        /// forward. Returns the module manifest.
        fn initialize(self: &Module) -> JsonBlob;

        /// Returns the interface definition.
        fn get_interface(self: &Module, interface_name: &str) -> JsonBlob;

        /// Registers the callback of the `Subscriber` to be called and calls
        /// `Everest::Module::signal_ready`.
        fn signal_ready(self: &Module, rt: Pin<&Runtime>);

        /// Informs the runtime that we implement the command described by `implementation_id` and
        /// `name`, and registers the `handle_command` method from the `Subscriber` as the handler.
        fn provide_command(
            self: &Module,
            rt: Pin<&Runtime>,
            implementation_id: String,
            name: String,
        );

        /// Call the command described by 'implementation_id' and `name` with the given 'args'.
        /// Returns the return value.
        fn call_command(
            self: &Module,
            implementation_id: &str,
            name: &str,
            args: JsonBlob,
        ) -> JsonBlob;

        /// Informs the runtime that we want to receive the variable described by
        /// `implementation_id` and `name` and registers the `handle_variable` method from the
        /// `Subscriber` as the handler.
        fn subscribe_variable(
            self: &Module,
            rt: Pin<&Runtime>,
            implementation_id: String,
            name: String,
        );

        /// Publishes the given `blob` under the `implementation_id` and `name`.
        fn publish_variable(self: &Module, implementation_id: &str, name: &str, blob: JsonBlob);

        /// Returns the module config from cpp.
        fn get_module_configs(module_id: &str, prefix: &str, conf: &str) -> Vec<RsModuleConfig>;

        /// Logging sink for the EVerest module.
        fn log2cxx(level: i32, line: i32, file: &str, message: &str);
    }
}

impl ffi::JsonBlob {
    fn as_bytes(&self) -> &[u8] {
        &self.data
    }

    fn deserialize<T: DeserializeOwned>(self) -> T {
        // TODO(hrapp): Error handling
        serde_json::from_slice(self.as_bytes()).unwrap()
    }

    fn from_vec(data: Vec<u8>) -> Self {
        Self { data }
    }
}

/// Very simple logger to use by the Rust modules.
mod logger {
    use super::ffi;
    use std::env;

    pub(crate) struct Logger {
        level: log::Level,
    }

    impl log::Log for Logger {
        fn enabled(&self, metadata: &log::Metadata) -> bool {
            // Rust gives the Error level 1 and all other severities a higher
            // value.
            metadata.level() <= self.level
        }

        fn log(&self, record: &log::Record) {
            // The doc says `log` has to perform the filtering itself.
            if !self.enabled(record.metadata()) {
                return;
            }
            // This mapping should be kept in sync with liblog's
            // Everest::Logging::severity_level.
            let level = match record.level() {
                log::Level::Trace => 0,
                log::Level::Debug => 1,
                log::Level::Info => 2,
                log::Level::Warn => 3,
                log::Level::Error => 4,
            };

            ffi::log2cxx(
                level,
                record.line().unwrap_or_default() as i32,
                record.file().unwrap_or_default(),
                &format!("{}", record.args()),
            )
        }

        fn flush(&self) {}
    }

    impl Logger {
        /// Init the logger for everest.
        ///
        /// Don't do this on your own as we must also control some cxx code.
        /// However, you can set the severity through the env-var
        /// `EVERESTRS_SEVERITY` to pre-filter logs from Rust. This will reduce
        /// the number of ffi calls and improve performance.
        pub(crate) fn init_logger() {
            let severity = env::var("EVERESTRS_SEVERITY")
                .unwrap_or("info".to_string())
                .to_lowercase();

            // Map the string to the severity.
            let level = match severity.as_str() {
                "trace" | "verb" => log::Level::Trace,
                "debug" | "debg" => log::Level::Debug,
                "info" => log::Level::Info,
                "warn" | "warning" => log::Level::Warn,
                _ => log::Level::Error,
            };

            let logger = Self { level };
            log::set_boxed_logger(Box::new(logger)).unwrap();
            log::set_max_level(level.to_level_filter());
        }
    }
}

/// The cpp_module is for Rust an opaque type - so Rust can't tell if it is safe
/// to be accessed from multiple threads. We know that the c++ runtime is meant
/// to be used concurrently.
unsafe impl Sync for ffi::Module {}
unsafe impl Send for ffi::Module {}

/// Arguments for an EVerest node.
#[derive(FromArgs, Debug)]
struct Args {
    /// prefix of installation.
    #[argh(option)]
    #[allow(unused)]
    pub prefix: PathBuf,

    /// configuration yml that we are running.
    #[argh(option)]
    #[allow(unused)]
    pub conf: PathBuf,

    /// module name for us.
    #[argh(option)]
    pub module: String,
}

/// Implements the handling of commands & variables, but has no specific information about the
/// details of the current module, i.e. it deals with JSON blobs and strings as command names. Code
/// generation is used to build the concrete, strongly typed abstractions that are then used by
/// final implementors.
pub trait Subscriber: Sync + Send {
    /// Handler for the command `name` on `implementation_id` with the given `parameters`. The return value
    /// will be returned as the result of the call.
    fn handle_command(
        &self,
        implementation_id: &str,
        name: &str,
        parameters: HashMap<String, serde_json::Value>,
    ) -> Result<serde_json::Value>;

    /// Handler for the variable `name` on `implementation_id` with the given `value`.
    fn handle_variable(
        &self,
        implementation_id: &str,
        name: &str,
        value: serde_json::Value,
    ) -> Result<()>;

    fn on_ready(&self) {}
}

/// The [Runtime] is the central piece of the bridge between c++ and Rust. We
/// have to ensure that the `cpp_module` never outlives the [Runtime] object.
/// This means that the [Runtime] **must** take ownership of `cpp_module`.
///
/// The `Subscriber` is not owned by the [Runtime] - in fact in derived user
/// code the `Subscriber` might take ownership of the [Runtime] - the weak
/// ownership hence is necessary to break possible ownership cycles.
pub struct Runtime {
    cpp_module: cxx::UniquePtr<ffi::Module>,
    sub_impl: RwLock<Option<Weak<dyn Subscriber>>>,
}

impl Runtime {
    fn on_ready(&self) {
        self.sub_impl
            .read()
            .unwrap()
            .as_ref()
            .unwrap()
            .upgrade()
            .unwrap()
            .on_ready();
    }

    fn handle_command(&self, impl_id: &str, name: &str, json: ffi::JsonBlob) -> ffi::JsonBlob {
        debug!("handle_command: {impl_id}, {name}, '{:?}'", json.as_bytes());
        let parameters: Option<HashMap<String, serde_json::Value>> = json.deserialize();
        let blob = self
            .sub_impl
            .read()
            .unwrap()
            .as_ref()
            .unwrap()
            .upgrade()
            .unwrap()
            .handle_command(impl_id, name, parameters.unwrap_or_default())
            .unwrap();
        ffi::JsonBlob::from_vec(serde_json::to_vec(&blob).unwrap())
    }

    fn handle_variable(&self, impl_id: &str, name: &str, json: ffi::JsonBlob) {
        debug!(
            "handle_variable: {impl_id}, {name}, '{:?}'",
            json.as_bytes()
        );
        self.sub_impl
            .read()
            .unwrap()
            .as_ref()
            .unwrap()
            .upgrade()
            .unwrap()
            .handle_variable(impl_id, name, json.deserialize())
            .unwrap();
    }

    pub fn publish_variable<T: serde::Serialize>(
        &self,
        impl_id: &str,
        var_name: &str,
        message: &T,
    ) {
        let blob = ffi::JsonBlob::from_vec(
            serde_json::to_vec(&message).expect("Serialization of data cannot fail."),
        );
        (self.cpp_module)
            .as_ref()
            .unwrap()
            .publish_variable(impl_id, var_name, blob);
    }

    pub fn call_command<T: serde::Serialize, R: serde::de::DeserializeOwned>(
        &self,
        impl_id: &str,
        name: &str,
        args: &T,
    ) -> R {
        let blob = ffi::JsonBlob::from_vec(
            serde_json::to_vec(args).expect("Serialization of data cannot fail."),
        );
        let return_value = (self.cpp_module)
            .as_ref()
            .unwrap()
            .call_command(impl_id, name, blob);
        serde_json::from_slice(&return_value.data).unwrap()
    }

    // TODO(hrapp): This function could use some error handling.
    pub fn new() -> Pin<Arc<Self>> {
        let args: Args = argh::from_env();
        let cpp_module = ffi::create_module(
            &args.module,
            &args.prefix.to_string_lossy(),
            &args.conf.to_string_lossy(),
        );

        logger::Logger::init_logger();

        Arc::pin(Self {
            cpp_module,
            sub_impl: RwLock::new(None),
        })
    }

    pub fn set_subscriber(self: Pin<&Self>, sub_impl: Weak<dyn Subscriber>) {
        *self.sub_impl.write().unwrap() = Some(sub_impl);
        let manifest_json = self.cpp_module.as_ref().unwrap().initialize();
        let manifest: schema::Manifest = manifest_json.deserialize();

        // Implement all commands for all of our implementations, dispatch everything to the
        // Subscriber.
        for (implementation_id, implementation) in manifest.provides {
            let interface_s = self.cpp_module.get_interface(&implementation.interface);
            let interface: schema::Interface = interface_s.deserialize();
            for (name, _) in interface.cmds {
                self.cpp_module.as_ref().unwrap().provide_command(
                    self,
                    implementation_id.clone(),
                    name,
                );
            }
        }

        // Subscribe to all variables that might be of interest.
        // TODO(hrapp): This looks very similar to the block above.
        for (implementation_id, provides) in manifest.requires {
            let interface_s = self.cpp_module.get_interface(&provides.interface);
            let interface: schema::Interface = interface_s.deserialize();
            for (name, _) in interface.vars {
                self.cpp_module.as_ref().unwrap().subscribe_variable(
                    self,
                    implementation_id.clone(),
                    name,
                );
            }
        }

        // Since users can choose to overwrite `on_ready`, we can call signal_ready right away.
        // TODO(hrapp): There were some doubts if this strategy is too inflexible, discuss design
        // again.
        (self.cpp_module).as_ref().unwrap().signal_ready(self);
    }
}

/// A store for our config values. The type is closely related to
/// [ffi::ConfigField] and [ffi::ConfigType].
#[derive(Debug)]
pub enum Config {
    Boolean(bool),
    String(String),
    Number(f64),
    Integer(i64),
}

impl TryFrom<&Config> for bool {
    type Error = Error;
    fn try_from(value: &Config) -> std::result::Result<Self, Self::Error> {
        match value {
            Config::Boolean(value) => Ok(*value),
            _ => Err(Error::MismatchedType(format!("{:?}", value))),
        }
    }
}

impl TryFrom<&Config> for String {
    type Error = Error;
    fn try_from(value: &Config) -> std::result::Result<Self, Self::Error> {
        match value {
            Config::String(value) => Ok(value.clone()),
            _ => Err(Error::MismatchedType(format!("{:?}", value))),
        }
    }
}

impl TryFrom<&Config> for f64 {
    type Error = Error;
    fn try_from(value: &Config) -> std::result::Result<Self, Self::Error> {
        match value {
            Config::Number(value) => Ok(*value),
            _ => Err(Error::MismatchedType(format!("{:?}", value))),
        }
    }
}

impl TryFrom<&Config> for i64 {
    type Error = Error;
    fn try_from(value: &Config) -> std::result::Result<Self, Self::Error> {
        match value {
            Config::Integer(value) => Ok(*value),
            _ => Err(Error::MismatchedType(format!("{:?}", value))),
        }
    }
}

/// Interface for fetching the configurations through the C++ runtime.
pub fn get_module_configs() -> HashMap<String, HashMap<String, Config>> {
    let args: Args = argh::from_env();
    let raw_config = ffi::get_module_configs(
        &args.module,
        &args.prefix.to_string_lossy(),
        &args.conf.to_string_lossy(),
    );

    // Convert the nested Vec's into nested HashMaps.
    let mut out: HashMap<String, HashMap<String, Config>> = HashMap::new();
    for mm_config in raw_config {
        let cc_config = mm_config
            .data
            .into_iter()
            .map(|field| {
                let value = match field.config_type {
                    ffi::ConfigType::Boolean => Config::Boolean(field.bool_value),
                    ffi::ConfigType::String => Config::String(field.string_value),
                    ffi::ConfigType::Number => Config::Number(field.number_value),
                    ffi::ConfigType::Integer => Config::Integer(field.integer_value),
                    _ => panic!("Unexpected value {:?}", field.config_type),
                };

                (field.name, value)
            })
            .collect::<HashMap<_, _>>();

        // If we have already an entry with the `module_name`, we try to extend
        // it.
        out.entry(mm_config.module_name)
            .or_default()
            .extend(cc_config);
    }

    out
}
