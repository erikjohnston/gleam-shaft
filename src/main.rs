#[macro_use]
extern crate slog;
#[macro_use]
extern crate clap;

use clap::Arg;
use daemonize::Daemonize;
use sloggers::Config;

use std::error::Error;
use std::fs::File;
use std::io::Read;
use std::process::exit;

use shaft::db::SqliteDatabase;
use shaft::rest::{
    format_pence_as_pounds_helper, register_servlets, AppConfig, AppState, AuthenticateUser,
    MiddlewareLogger,
};
use shaft::settings::Settings;

/// Attempts to load and build the handlebars template file.
macro_rules! load_template {
    ($logger:expr, $hb:expr, $root:expr, $name:expr) => {
        if let Err(e) = load_template_impl(&mut $hb, $root, $name) {
            crit!($logger, "Failed to load resources/{}.hbs: {}", $name, e);
            exit(1);
        }
    };
}

/// App Entry point.
fn main() {
    // Load settings, first by looking at command line options for config files
    // to look in.
    let matches = clap::app_from_crate!()
        .arg(
            Arg::with_name("config")
                .short("c")
                .multiple(true)
                .long("config")
                .value_name("FILE")
                .help("Sets a custom config file")
                .takes_value(true)
                .required(false),
        )
        .get_matches();

    let mut c = config::Config::new();

    // We can have multiple config files which get merged together
    for file in matches.values_of("config").unwrap_or_default() {
        if let Err(err) = c.merge(config::File::with_name(file)) {
            // We don't have a logger yet, so print to stderr
            eprintln!("{}", err);
            exit(1)
        }
    }

    // Also load config from environment
    c.merge(config::Environment::with_prefix("SHAFT")).unwrap();

    let settings: Settings = match c.try_into() {
        Ok(s) => s,
        Err(err) => {
            // We don't have a logger yet, so print to stderr
            eprintln!("Config error: {}", err);
            exit(1);
        }
    };

    // Set up logging immediately.
    let logger = settings.log.build_logger().unwrap();

    // Load and build the templates.
    let mut hb = handlebars::Handlebars::new();
    load_template!(logger, hb, &settings.resource_dir, "index");
    load_template!(logger, hb, &settings.resource_dir, "login");
    load_template!(logger, hb, &settings.resource_dir, "transactions");
    load_template!(logger, hb, &settings.resource_dir, "base");
    hb.register_helper("pence-as-pounds", Box::new(format_pence_as_pounds_helper));

    // Set up the database
    let database = SqliteDatabase::with_path(settings.database_file);

    // Sanitize the webroot to not end in a trailing slash.
    let web_root = settings.web_root.trim_end_matches('/').to_string();

    // This is the read only config for the app.
    let app_config = AppConfig {
        github_client_id: settings.github.client_id.clone(),
        github_client_secret: settings.github.client_secret.clone(),
        github_state: settings.github.state.clone(),
        web_root,
        required_org: settings.github.required_org.clone(),
        resource_dir: settings.resource_dir.clone(),
    };

    // Holds the state for the shared state of the app. Gets cloned to each thread.
    let app_state = AppState::new(app_config, hb, database);

    // Set up HTTP server
    let mut sys = actix_rt::System::new("shaft"); // Need to set up an actix system first.

    let logger_middleware = MiddlewareLogger::new(logger.clone());

    let http_server = actix_web::HttpServer::new(move || {
        // This gets called in each thread to set up the HTTP handlers

        let logger_middleware = logger_middleware.clone();

        actix_web::App::new()
            .data(app_state.clone())
            .app_data(app_state.clone())
            .wrap(AuthenticateUser::new(app_state.database.clone()))
            .wrap_fn(move |req, srv| logger_middleware.wrap(req, srv))
            .configure(|config| register_servlets(config, &app_state))
    })
    .bind(&settings.bind)
    .unwrap();

    // If we need to daemonize do so *just* before starting the event loop
    if let Some(daemonize_settings) = settings.daemonize {
        Daemonize::new()
            .pid_file(daemonize_settings.pid_file)
            .start()
            .expect("be able to daemonize");
    }

    // Start the event loop.
    info!(logger, "Started server on http://{}", settings.bind);
    let _ = sys.block_on(async move { http_server.run().await });
}

/// Attempts to load the template into handlebars instance.
fn load_template_impl(
    hb: &mut handlebars::Handlebars,
    root: &str,
    name: &str,
) -> Result<(), Box<dyn Error>> {
    let mut index_file = File::open(format!("{}/{}.hbs", root, name))?;
    let mut source = String::new();
    index_file.read_to_string(&mut source)?;
    hb.register_template_string(name, source)?;

    Ok(())
}
