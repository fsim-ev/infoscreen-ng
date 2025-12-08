use std::{
	sync::{Arc, Once},
	collections::{BTreeSet, HashMap},
	thread, time, path::PathBuf,
};

use clap::Parser;
use slint::{self,
	ComponentHandle, Weak,
	Model, VecModel,
	Color, SharedString,
};

use tokio;
use tokio_util::sync::CancellationToken;
use reqwest as http;
use futures_util::TryFutureExt;

use feed_rs;
use bytes::Buf;
use chrono::{prelude::*, Duration};
use serde::{Deserialize, Serialize};
use regex::{self, Regex};
use figment::{Figment, providers::{Format, Serialized, Toml}};
use xdg;

use anyhow::{Result, Context};
use sys_locale;

use tracing as log;
use tracing_subscriber;

mod untis;
mod othr_ptp;
mod ui {
	slint::include_modules!();
}

#[derive(Parser, Debug, Default, Deserialize, Serialize)]
#[clap(about, version)]
struct Options
{
	/// Config path
	#[clap(short = 'C', long = "config", default_value = "./config.toml")]
	config_path: PathBuf,

	/// Offset in days
	#[clap(short = 'o', long = "offset", value_parser, default_value_t = 0)]
	day_offset: i16,
}


#[derive(Debug, Deserialize, Serialize)]
struct Config {
	day_offset: i16,
	time_off: TimeRange,

	untis: Option<UntisConfig>,
	newsfeed_url: Option<String>,

	course_colors: ColorSet,
	location_colors: ColorSet,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct TimeRange {
	start: u32,
	end: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct UntisConfig {
	school: String,
	auth: Auth,
	faculty: String,
	day_range: u16,
}

impl Default for UntisConfig {
	fn default() -> Self {
	    Self {
	    	school: Default::default(),
	    	auth: Default::default(),
	    	faculty: "IM".into(),
	    	day_range: 3,
	    }
	}
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct Auth {
	username: String,
	password: String,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct ColorSet {
	default: u32,
	map: HashMap<String, u32>,
}

impl Default for Config {
	fn default() -> Self {
		Self {
			day_offset: 0,
			time_off: TimeRange { start: 1900, end: 0300 },

			untis: Default::default(),
			newsfeed_url: Default::default(),

			course_colors: ColorSet {
				default: 0xFF_666666,
				map: Default::default(),
			},

            location_colors: ColorSet {
            	default: 0xFF_666666,
				map: Default::default(),
			},
		}
	}
}




fn run<'de, Runner, Ui, Options, Config>(ui: Ui, opts: Options, runner: Runner) -> anyhow::Result<()>
	where
		Ui: ComponentHandle, 
		Options: Parser + Deserialize<'de> + Serialize,
		Config: Default + Deserialize<'de> + Serialize + core::fmt::Debug,
		Runner: FnOnce(Weak<Ui>, Config, CancellationToken) -> Result<()>,
{
	let opts = Options::parse();
	let xdg_dirs = xdg::BaseDirectories::with_prefix("infoscreen")?;

	let fig = Figment::new()
		.merge(Serialized::defaults(Config::default()))
	    .merge(Toml::file(&xdg_dirs.get_config_file("timetable.toml")))
	    .merge(Toml::file(&opts.config_path))
	    .merge(Serialized::defaults(opts));

	let config: Config = fig.extract()?;

	tracing_subscriber::fmt()
		.with_env_filter(tracing_subscriber::EnvFilter::from_default_env()
			.add_directive(tracing_subscriber::filter::LevelFilter::INFO.into())
			.add_directive("infoscreen_timetable=debug".parse()?)
			.add_directive("reqwest=debug".parse()?)
			//.add_directive("hyper=trace".parse()?)
		)
		.compact()
		.init();

	fig.metadata().for_each(|md| {
		if let Some(src) = md.source.as_ref() {
			log::debug!("using config from {} - {}", md.name, src);
		}
	});

	log::debug!("config: {:?}", &config);

	let io_task_run = CancellationToken::new();
	let io_task_handle = thread::Builder::new()
		.name("io-runtime".into())
		.spawn({
			let ui = ui.as_weak();
			let run_token = io_task_run.child_token();
			move || runner(ui, config, run_token).expect("fatal error")
		})?;

	let cleanup_task_handle = thread::Builder::new()
		.name("cleanup".into())
		.spawn({
			let ui = ui.as_weak();
			move || {
				log::debug!("awaiting IO task to finish");
				if let Err(panic) = io_task_handle.join()
				{
					let err_str = panic_description(panic);
					log::error!("PANIC! {}", err_str);
					thread::sleep(time::Duration::from_secs(1));
					ui.upgrade_in_event_loop(move |ui| {
						ui.set_timetable_status(err_str.into());
					}).ok();
					thread::sleep(time::Duration::from_secs(10));
				} else {
					log::error!("failed to join IO thread");
				}
				slint::invoke_from_event_loop(move || { slint::quit_event_loop().ok(); }).ok();
			}
		})?;

	ui.run();
	log::debug!("UI task shutting down...");
	io_task_run.cancel();
	cleanup_task_handle.join().expect("failed to join cleanup task");

	log::info!("Have a nice day!");
	Ok(())
}


fn main() -> Result<(), Box<dyn std::error::Error>>
{
	let opts = Options::parse();
	let xdg_dirs = xdg::BaseDirectories::with_prefix("infoscreen")?;

	let fig = Figment::new()
		.merge(Serialized::defaults(Config::default()))
	    .merge(Toml::file(&xdg_dirs.get_config_file("timetable.toml")))
	    .merge(Toml::file(&opts.config_path))
	    .merge(Serialized::defaults(opts));

	let config: Config = fig.extract()?;

	tracing_subscriber::fmt()
		.with_env_filter(tracing_subscriber::EnvFilter::from_default_env()
			.add_directive(tracing_subscriber::filter::LevelFilter::INFO.into())
			.add_directive("infoscreen_timetable=debug".parse()?)
			.add_directive("reqwest=debug".parse()?)
			//.add_directive("hyper=trace".parse()?)
		)
		.compact()
		.init();

	fig.metadata().for_each(|md| {
		if let Some(src) = md.source.as_ref() {
			log::debug!("using config from {} - {}", md.name, src);
		}
	});

	log::debug!("config: {:?}", &config);

	let ui = ui::App::new();
	let io_task_run = CancellationToken::new();
	let io_task_handle = thread::Builder::new()
		.name("io-runtime".into())
		.spawn({
			let ui = ui.as_weak();
			let run_token = io_task_run.child_token();
			move || io_runtime_run(ui, config, run_token).expect("fatal error")
		})?;

	let cleanup_task_handle = thread::Builder::new()
		.name("cleanup".into())
		.spawn({
			let ui = ui.as_weak();
			move || {
				log::debug!("awaiting IO task to finish");
				if let Err(panic) = io_task_handle.join()
				{
					let err_str = panic_description(panic);
					log::error!("PANIC! {}", err_str);
					thread::sleep(time::Duration::from_secs(1));
					ui.upgrade_in_event_loop(move |ui| {
						ui.set_timetable_status(err_str.into());
					}).ok();
					thread::sleep(time::Duration::from_secs(10));
				} else {
					log::error!("failed to join IO thread");
				}
				slint::invoke_from_event_loop(move || { slint::quit_event_loop().ok(); }).ok();
			}
		})?;

	ui.run();
	log::debug!("UI task shutting down...");
	io_task_run.cancel();
	cleanup_task_handle.join().expect("failed to join cleanup task");

	log::info!("Have a nice day!");
	Ok(())
}

fn panic_description(panic: Box<dyn std::any::Any + Send>) -> String {
	panic.downcast::<String>()
		.map(|str_box| *str_box)
		.or_else(|panic| panic.downcast::<&str>().map(|str_box| str_box.to_string()))
		.unwrap_or_else(|panic| format!("unknown panic type: {:?}", panic))
}

fn io_runtime_run(ui: Weak<ui::App>, conf: Config, run_token: CancellationToken) -> Result<()>
{
	use tokio::*;

	let rt = runtime::Builder::new_multi_thread()
		.enable_all()
		.thread_name("io-worker")
		.build()?;

	rt.block_on(io_run(ui, conf, run_token))?;
	rt.shutdown_timeout(time::Duration::from_secs(10));

	Ok(())
}

async fn io_run(ui: Weak<ui::App>, mut conf: Config, run_token: CancellationToken) -> Result<()>
{
	Ok(())
}