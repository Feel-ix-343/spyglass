extern crate notify;
//use crate::importer::FirefoxImporter;
use std::io;
use std::time::Duration;
use tokio::signal;
use tokio::sync::{broadcast, mpsc};
use tracing_log::LogTracer;
use tracing_subscriber::{fmt, layer::SubscriberExt, EnvFilter};

use entities::models::{crawl_queue, lens};
use libspyglass::pipeline;
use libspyglass::plugin;
use libspyglass::state::AppState;
use libspyglass::task::{self, AppShutdown, Command};
#[allow(unused_imports)]
use migration::Migrator;
use shared::config::Config;

mod api;

#[cfg(not(debug_assertions))]
const LOG_LEVEL: tracing::Level = tracing::Level::INFO;

#[cfg(debug_assertions)]
const LOG_LEVEL: tracing::Level = tracing::Level::DEBUG;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::new();

    #[cfg(not(debug_assertions))]
    let _guard = if config.user_settings.disable_telemetry {
        None
    } else {
        Some(sentry::init((
            "https://5c1196909a4e4e5689406705be13aad3@o1334159.ingest.sentry.io/6600345",
            sentry::ClientOptions {
                release: sentry::release_name!(),
                ..Default::default()
            },
        )))
    };

    let file_appender = tracing_appender::rolling::daily(config.logs_dir(), "server.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    let subscriber = tracing_subscriber::registry()
        .with(
            EnvFilter::from_default_env()
                .add_directive(LOG_LEVEL.into())
                .add_directive("tantivy=WARN".parse().expect("Invalid EnvFilter"))
                .add_directive("regalloc=WARN".parse().expect("Invalid EnvFilter"))
                .add_directive("cranelift_codegen=WARN".parse().expect("Invalid EnvFilter"))
                .add_directive("wasmer_wasi=WARN".parse().expect("Invalid EnvFilter"))
                .add_directive(
                    "wasmer_compiler_cranelift=WARN"
                        .parse()
                        .expect("Invalid EnvFilter"),
                )
                .add_directive("docx=WARN".parse().expect("Invalid EnvFilter")),
        )
        .with(fmt::Layer::new().with_writer(io::stdout))
        .with(fmt::Layer::new().with_ansi(false).with_writer(non_blocking));

    tracing::subscriber::set_global_default(subscriber).expect("Unable to set a global subscriber");
    LogTracer::init()?;

    log::info!("Loading prefs from: {:?}", Config::prefs_dir());
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("spyglass-backend")
        .build()
        .expect("Unable to create tokio runtime");

    // Run any migrations, only on headless mode.
    #[cfg(debug_assertions)]
    {
        let migration_status = rt.block_on(async {
            match Migrator::run_migrations().await {
                Ok(_) => Ok(()),
                Err(e) => {
                    let msg = e.to_string();
                    // This is ok, just the migrator being funky
                    if !msg.contains("been applied but its file is missing") {
                        // Ruh-oh something went wrong
                        log::error!("Unable to migrate database - {}", e.to_string());
                        // Exit from app
                        return Err(());
                    }

                    Ok(())
                }
            }
        });

        if migration_status.is_err() {
            return Ok(());
        }
    }

    // Initialize/Load user preferences
    let mut state = rt.block_on(AppState::new(&config));
    rt.block_on(start_backend(&mut state, &config));

    Ok(())
}

async fn start_backend(state: &mut AppState, config: &Config) {
    // Initialize crawl_queue, requeue all in-flight tasks.
    crawl_queue::reset_processing(&state.db).await;
    if let Err(e) = lens::reset(&state.db).await {
        log::error!("Unable to reset lenses: {}", e);
    }

    // Create channels for scheduler / crawlers
    let (crawl_queue_tx, crawl_queue_rx) = mpsc::channel(
        state
            .user_settings
            .inflight_crawl_limit
            .value()
            .try_into()
            .expect("Unable to parse inflight_crawl_limit"),
    );

    // Channel for shutdown listeners
    let (shutdown_tx, _) = broadcast::channel::<AppShutdown>(16);
    // Channel for crawle cmds
    let (crawler_tx, _) = broadcast::channel::<Command>(16);
    // Channel for plugin commands
    let (plugin_cmd_tx, plugin_cmd_rx) = mpsc::channel(16);

    let (pipeline_cmd_tx, pipeline_cmd_rx) = mpsc::channel(16);

    // Loads and processes pipeline commands
    let _pipeline_handler = tokio::spawn(pipeline::initialize_pipelines(
        state.clone(),
        config.clone(),
        pipeline_cmd_rx,
        shutdown_tx.clone(),
    ));

    {
        state
            .crawler_cmd_tx
            .lock()
            .await
            .replace(crawler_tx.clone());
    }

    {
        state
            .plugin_cmd_tx
            .lock()
            .await
            .replace(plugin_cmd_tx.clone());
    }

    {
        state
            .pipeline_cmd_tx
            .lock()
            .await
            .replace(pipeline_cmd_tx.clone());
    }

    // Check lenses for updates & add any bootstrapped URLs to crawler.
    let lens_watcher_handle = tokio::spawn(task::lens_watcher(
        state.clone(),
        config.clone(),
        crawler_tx.subscribe(),
        shutdown_tx.subscribe(),
    ));

    // Crawl scheduler
    let manager_handle = tokio::spawn(task::manager_task(
        state.clone(),
        crawl_queue_tx,
        crawler_tx.subscribe(),
        shutdown_tx.subscribe(),
    ));

    // Crawlers
    let worker_handle = tokio::spawn(task::worker_task(
        state.clone(),
        crawl_queue_rx,
        crawler_tx.subscribe(),
        shutdown_tx.subscribe(),
    ));

    // Clean up crew. Commit anything added to the index in the last 10s
    {
        let state = state.clone();
        let _ = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(10));

            loop {
                interval.tick().await;
                if let Err(err) = state
                    .index
                    .writer
                    .lock()
                    .expect("Unable to get index lock")
                    .commit()
                {
                    log::error!("commit loop error: {:?}", err);
                }
            }
        });
    }

    // Plugin server
    let pm_handle = tokio::spawn(plugin::plugin_event_loop(
        state.clone(),
        config.clone(),
        plugin_cmd_tx.clone(),
        plugin_cmd_rx,
        shutdown_tx.subscribe(),
    ));

    // API server
    let api_server = tokio::spawn(api::start_api_server(state.clone()));

    // Gracefully handle shutdowns
    match signal::ctrl_c().await {
        Ok(()) => {
            lens_watcher_handle.abort();
            pm_handle.abort();
            log::warn!("Shutdown request received");
            shutdown_tx
                .send(AppShutdown::Now)
                .expect("Unable to send AppShutdown cmd");
        }
        Err(err) => {
            log::error!("Unable to listen for shutdown signal: {}", err);
            shutdown_tx
                .send(AppShutdown::Now)
                .expect("Unable to send AppShutdown cmd");
        }
    }

    let _ = tokio::join!(manager_handle, worker_handle, pm_handle, api_server);
}
