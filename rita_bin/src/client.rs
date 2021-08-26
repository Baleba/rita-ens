//! This is the main source file for the Rita client binary, by 'client' we mean 'not an exit server'
//! all meshing and billing functionality is contained in `rita_common` and is common to both rita and
//! `rita_exit`. The major difference is billing and connection code for the 'exit', the mandatory
//! vpn system integrated into the Althea network design, as well as API endpoints for a management
//! dashboard of router functions like wifi, which the exit is not expected to have.
//!
//! This file initializes the dashboard endpoints for the client as well as the common and client
//! specific actors.

#![warn(clippy::all)]
#![allow(clippy::pedantic)]
#![forbid(unsafe_code)]

#[cfg(feature = "jemalloc")]
use jemallocator::Jemalloc;
#[cfg(feature = "jemalloc")]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

#[macro_use]
extern crate lazy_static;
#[macro_use]
extern crate log;

use althea_kernel_interface::KernelInterface;
use althea_kernel_interface::LinuxCommandRunner;
use docopt::Docopt;
use rita_client::dashboard::start_client_dashboard;
use rita_client::get_client_usage;
use rita_client::logging::enable_remote_logging;
use rita_client::rita_loop::check_rita_client_actors;
use rita_client::rita_loop::start_rita_client_endpoints;
use rita_client::wait_for_settings;
use rita_client::Args;
use rita_common::rita_loop::check_rita_common_actors;
use rita_common::rita_loop::start_core_rita_endpoints;
use rita_common::utils::env_vars_contains;
use settings::client::{OldRitaClientSettings, RitaClientSettings};
use settings::FileWrite;
use std::env;

lazy_static! {
    pub static ref KI: Box<dyn KernelInterface> = Box::new(LinuxCommandRunner {});
}

fn main() {
    info!("Started this main function");

    let args: Args = Docopt::new(get_client_usage(
        env!("CARGO_PKG_VERSION"),
        env!("GIT_HASH"),
    ))
    .and_then(|d| d.deserialize())
    .unwrap_or_else(|e| e.exit());

    let settings_file = args.flag_config;
    wait_for_settings(&settings_file);

    // load the settings file, setup a thread to save it out every so often
    // and populate the memory cache of settings used throughout the program
    let settings: RitaClientSettings = {
        let platform = &args.flag_platform;

        OldRitaClientSettings::new_watched(&settings_file).unwrap();
        let mut s = settings::get_rita_client();

        settings::set_flag_config(settings_file.to_string());

        settings::set_git_hash(env!("GIT_HASH").to_string());

        s.set_future(args.flag_future);

        let s = clu::init(platform, s);

        s.write(&settings_file).unwrap();
        settings::set_rita_client(s.clone());
        println!("Look the client settings! {:?}", s);
        s
    };

    // On Linux static builds we need to probe ssl certs path to be able to
    // do TLS stuff.
    openssl_probe::init_ssl_cert_env_vars();

    // we should remove log if there's an operator address or if logging is enabled
    let should_remote_log = settings.log.enabled || settings.operator.operator_address.is_some();
    // if remote logging is disabled, or the NO_REMOTE_LOG env var is set we should use the
    // local logger and log to std-out. Note we don't care what is actually set in NO_REMOTE_LOG
    // just that it is set
    if !should_remote_log || env_vars_contains("NO_REMOTE_LOG") {
        env_logger::init();
    } else {
        let res = enable_remote_logging();
        println!("logging status {:?}", res);
    }

    if cfg!(feature = "development") {
        println!("Warning!");
        println!("This build is meant only for development purposes.");
        println!("Running this on production is unsupported and not safe!");
    }

    // If we are an an OpenWRT device try and rescue it from update issues
    if KI.is_openwrt() && KI.check_cron().is_err() {
        error!("Failed to setup cron!");
    }

    info!(
        "crate ver {}, git hash {}",
        env!("CARGO_PKG_VERSION"),
        env!("GIT_HASH")
    );
    trace!("Starting with Identity: {:?}", settings.get_identity());

    let system = actix::System::new(format!("main {:?}", settings.network.mesh_ip));

    check_rita_common_actors();
    check_rita_client_actors();
    start_core_rita_endpoints(4);
    start_rita_client_endpoints(1);
    start_client_dashboard(settings.network.rita_dashboard_port);

    system.run();
    info!("Started Rita Client!");
}
