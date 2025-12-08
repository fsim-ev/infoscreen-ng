use std::{
	ops::Deref,
	path::PathBuf,
	sync::{Arc, OnceLock},
	thread, time,
};

use clap::Parser;
use slint::{
	self, Color, ComponentHandle, 
	Model, VecModel, Weak, SharedString, ModelRc
};

use reqwest as http;
use tokio;
use tokio_util::sync::CancellationToken;

use chrono::{prelude::*, Duration};
use figment::{
	providers::{Format, Serialized, Toml},
	Figment,
};
use serde::{Deserialize, Deserializer, Serialize};
use icalendar::{Component, EventLike, DatePerhapsTime};
use xdg;

use anyhow::{Context, Result, ensure};
use sys_locale;

use tracing as log;
use tracing_subscriber;

mod ui {
	slint::include_modules!();
}

#[derive(Parser, Debug, Serialize)]
#[clap(about, version)]
struct Options {
	/// Config path
	#[clap(short = 'C', long = "config", default_value = "./config.toml")]
	config_path: PathBuf,

	/// Schedule refresh interval
	#[clap(short = 'r', long = "refresh", default_value_t = 120)]
	update_rate: u32,
}

#[derive(Debug, Deserialize, Serialize)]
struct Config {
	update_rate: u32,

	calendar: Vec<CalendarOpt>,
}

impl Default for Config {
	fn default() -> Self {
	    Self {
	    	update_rate: 120,
	    	calendar: Default::default(),
	    }
	}
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CalendarOpt {
	name: String,
	color: Option<u32>,
	url: String,
	auth: Option<Auth>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct Auth {
	username: String,
	password: String,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
	// Config init
	let opts = Options::parse();
	let xdg_dirs = xdg::BaseDirectories::with_prefix("infoscreen")?;

	let fig = Figment::new()
		.merge(Serialized::defaults(Config::default()))
		.merge(Toml::file(&xdg_dirs.get_config_file("calendar.toml")))
		.merge(Toml::file(&opts.config_path))
		.merge(Serialized::defaults(opts));

	let conf: Config = fig.extract()?;

	// Logging system init
	tracing_subscriber::fmt()
		.with_env_filter(
			tracing_subscriber::EnvFilter::from_default_env()
				.add_directive(tracing_subscriber::filter::LevelFilter::INFO.into())
				.add_directive("infoscreen_todo_list=debug".parse()?)
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

	let ui = ui::App::new()?;
	ui.show()
		.context("failed to create window")?;
	let io_task_run = CancellationToken::new();
	let io_task_handle = thread::Builder::new().name("io-runtime".into()).spawn({
		let ui = ui.as_weak();
		let run_token = io_task_run.child_token();
		move || io_runtime_run(ui, conf, run_token).expect("fatal error")
	})?;

	let cleanup_task_handle = thread::Builder::new().name("cleanup".into()).spawn({
		let ui = ui.as_weak();
		move || {
			if let Err(panic) = io_task_handle.join() {
				let err_str = panic_description(panic);
				log::error!("PANIC! {}", err_str);
				thread::sleep(time::Duration::from_secs(1));
				ui.upgrade_in_event_loop(move |ui| {
					//ui.set_status(err_str.into());
				})
				.ok();
				thread::sleep(time::Duration::from_secs(10));
			}
			slint::invoke_from_event_loop(move || {
				slint::quit_event_loop().ok();
			})
			.ok();
		}
	})?;

	ui.run()
		.context("failed to start event loop")?;
	log::debug!("UI task shutting down...");
	io_task_run.cancel();
	cleanup_task_handle
		.join()
		.expect("failed to join cleanup task");

	log::info!("Have a nice day!");
	Ok(())
}

fn panic_description(panic: Box<dyn std::any::Any + Send>) -> String {
	panic
		.downcast::<String>()
		.map(|str_box| *str_box)
		.or_else(|panic| panic.downcast::<&str>().map(|str_box| str_box.to_string()))
		.unwrap_or_else(|panic| format!("unknown panic type: {:?}", panic))
}

fn io_runtime_run(ui: Weak<ui::App>, conf: Config, run_token: CancellationToken) -> Result<()> {
	use tokio::*;

	let rt = runtime::Builder::new_current_thread()
		.enable_all()
		.thread_name("io-worker")
		.build()?;

	rt.block_on(io_run(ui, conf, run_token))?;
	rt.shutdown_timeout(time::Duration::from_secs(10));

	Ok(())
}

async fn io_run(ui: Weak<ui::App>, conf: Config, run_token: CancellationToken) -> Result<()> {
	use tokio::*;

	ensure!(!conf.calendar.is_empty(), "no calendars configured!");

	let time_task = spawn({
		let ui = ui.clone();
		async move {
			let mut old_date = chrono::Local.timestamp_opt(0, 0).unwrap().date_naive();
			let mut old_time = chrono::Local.timestamp_opt(0, 0).unwrap().time();
			loop {
				let now = chrono::Local::now();
				let new_time = if now.time().minute() != old_time.minute() {
					let time = now.time();
					let time_str = time.format("%H:%M").to_string();
					old_time = time;
					Some(time_str)
				} else {
					None
				};
				let new_date = if now.date_naive() != old_date {
					let date = now.date_naive();
					// Remind about new year
					let date_fmt = if date.month() == 1 {
						"%A, %e. %B %Y"
					} else {
						"%A, %e. %B"
					};
					let date_str = date.format_localized(date_fmt, locale()).to_string();
					old_date = date;
					Some(date_str)
				} else {
					None
				};

				ui.upgrade_in_event_loop(move |ui| {
					ui.set_secs(now.second() as i32);
					if let Some(date) = new_date {
						ui.set_date(date.into());
					}
					if let Some(time) = new_time {
						ui.set_time(time.into());

						ui.global::<ui::State>()
							.set_time(now.timestamp() as _);

					}
				}).ok();

				time::sleep(time::Duration::from_millis(250)).await;
			}
		}
	});

	let (err_chan, mut err_rx) = sync::mpsc::channel::<anyhow::Result<()>>(8);
	let calopts = Arc::new(conf.calendar);

	let client = http::Client::builder()
		.connect_timeout(time::Duration::from_secs(10))
		.timeout(time::Duration::from_secs(20))
		.pool_idle_timeout(time::Duration::from_secs((conf.update_rate + 2) as _))
		.build()?;

	let schedule_updater = spawn({
		let ui = ui.clone();
		let calopts = calopts.clone();
		let err_chan = err_chan.clone();
		async move {
			ui.upgrade_in_event_loop(move |ui| {
				// prime type id for later
				//ui.set_events(Vec::<ui::Event>::default().deref().into());
				//ui.set_tasks(Vec::<ui::Task>::default().deref().into());

				fn sec_to_dt(sec: i32) -> Option<DateTime<Local>> {
					chrono::Utc.timestamp_opt(sec as _, 0)
						.single()
						.map(|dt| dt.with_timezone(&Local))
				}

				let state = ui.global::<ui::State>();
				state.on_fmt_time(|sec|
					sec_to_dt(sec)
						.map(|dt| dt.format_localized("%H:%M", locale()).to_string())
						.unwrap_or_default()
						.into()
				);
				state.on_fmt_date(|sec|
					sec_to_dt(sec)
						.map(|dt| dt.format_localized("%d.%m", locale()).to_string())
						.unwrap_or_default()
						.into()
				);
				state.on_is_midnight(|sec|
					sec_to_dt(sec)
						.map(|dt| dt.num_seconds_from_midnight() == 0)
						.unwrap_or_default()
				);
				state.on_time_in_days(|start, end|
					sec_to_dt(start).zip(sec_to_dt(end))
						.map(|(start, end)| (end - start).num_days() as _)
						.unwrap_or_default()
				);
			}).ok();

			let mut sleep_dur = time::Duration::ZERO;

			loop {
				if !sleep_dur.is_zero() {
					let now = Local::now();
					let dur = Duration::from_std(sleep_dur).unwrap();
					log::info!("next update at {} (in {} min)", (now + dur).to_rfc2822(), dur.num_minutes());
					ui.upgrade_in_event_loop(|ui| ui.set_loading(false));
				}		
				time::sleep(sleep_dur).await;	
				sleep_dur = time::Duration::from_secs(conf.update_rate as _);
				log::info!("updating calendars...");

				let now = chrono::Local::now();
				let mut events = Vec::new();
				let mut tasks = Vec::new();

				for calopt in calopts.iter() {
					log::debug!(calendar=calopt.name, "loading");
					let cal = match fetch_calendar(&client, calopt.url.parse().unwrap(), &calopt.auth).await {
						Ok(v) => v,
						Err(err) => {
							log::error!("failed to load calendar: {err}");
							continue;
						}
					};

					log::info!(calendar=calopt.name, event_num=cal.events.len(), task_num=cal.tasks.len(), "loaded");
			
					events.extend(cal.events.into_iter()
						.filter(|sec| (NaiveDateTime::from_timestamp_opt(sec.time_end as _, 0).unwrap() + Duration::days(1)) > (now.date_naive()).and_time(Default::default()))
						.inspect(|ev| log::debug!(start=ev.time_start, end=ev.time_end, /*summary=ev.summary,*/ "  event loaded"))
					);
					tasks.extend(cal.tasks.into_iter()
						//.filter(|task| task)
					);
				}

				events.sort_by(|a, b| a.time_start.cmp(&b.time_start));

				log::debug!(event_num=events.len(), task_num=tasks.len(), "updating UI... ");
				ui.upgrade_in_event_loop(move |ui| {
					fn set_list<T: 'static>(mrc: ModelRc<T>, list: Vec<T>) {
						let model = mrc.as_any()
							.downcast_ref::<VecModel<T>>()
							.expect("unexpected ui model");

						model.set_vec(list);
					}					
					set_list(ui.get_events(), events);
					//set_list(ui.get_tasks(), tasks);
				});
			}
		}
	});

	tokio::select! {
		res = schedule_updater => if let Err(err) = res {
			let err = if err.is_panic() {
				panic_description(err.into_panic())
			} else {
				err.to_string()
			};
			err_chan.try_send(Err(anyhow::format_err!("failed to update: {}", err))).ok();
		},
		_ = tokio::signal::ctrl_c() => {
			log::debug!("SIGINT detected!");
		},
		_ = time_task => {},
		_ = run_token.cancelled() => {},
	}

	log::debug!("IO task shutting down...");
	Ok(())
}

fn locale() -> chrono::Locale {
	static LOCALE: OnceLock<chrono::Locale> = OnceLock::new();
	LOCALE.get_or_init(|| {
		match sys_locale::get_locale()
			.ok_or("detect system locale".to_owned())
			.and_then(|locstr| chrono::Locale::try_from(locstr.replace('-', "_").as_str())
				.map_err(|_err| format!("parse system locale '{locstr}'"))
			)
		{
			Ok(loc) => loc,
			Err(err) => {
				log::error!("failed to {err}");
				chrono::Locale::POSIX
			},
		}
	}).to_owned()
}

fn try_into(comp: impl icalendar::EventLike) -> Result<ui::Event> {
	fn dt_convert(cdt: DatePerhapsTime) -> Result<i32> {
		match cdt {
			DatePerhapsTime::DateTime(dt) => 
				dt.try_into_utc()
					.context("failed to convert datetime to UTC"),
			DatePerhapsTime::Date(d) => 
				d.and_time(chrono::NaiveTime::from_hms_opt(0, 0, 0).unwrap())
					.and_local_timezone(chrono::Local)
					.map(|dt| dt.with_timezone(&Utc))
					.single()
					.context("failed to convert date to datetime"),
		}
		.map(|val| val.timestamp() as _)
	}

	let ev = ui::Event {
		summary: comp.get_summary().context("summary missing")?.into(),
		location: comp.get_location().unwrap_or_default().into(),
		categories: comp.property_value("CATEGORIES")
			.map(|s| s.replace(',', ", ").to_owned().into())
			.unwrap_or_default(),
		time_start: comp.get_start()
			.ok_or_else(|| anyhow::format_err!("missing start time"))
			.and_then(dt_convert)?,
		time_end: comp.get_end()
			.ok_or_else(|| anyhow::format_err!("missing start time"))
			.and_then(dt_convert)
			.unwrap_or_default(),
	    
	    organiser: Default::default(),
	};
	Ok(ev)
}

impl TryInto<ui::Event> for &icalendar::Event {
	type Error = anyhow::Error;

	fn try_into(self) -> Result<ui::Event> {
		try_into(self.clone())
	}
}

impl TryInto<ui::Event> for &icalendar::Todo {
	type Error = anyhow::Error;

	fn try_into(self) -> Result<ui::Event> {
		try_into(self.clone())
	}
}

struct Calendar {
	events: Vec<ui::Event>,
	tasks: Vec<ui::Event>,
}

async fn fetch_calendar(client: &http::Client, url: http::Url, auth: &Option<Auth>) -> Result<Calendar> {

	let mut req = client.get(url);
	if let Some(auth) = auth {
		req = req.basic_auth(&auth.username, Some(&auth.password))
	};

	let body = req.send()
		.await
		.and_then(|res| res.error_for_status())
		.context("failed to fetch headers")?
		.text()
		.await
		.map(|s| icalendar::parser::unfold(&s))
		.context("failed to fetch body")?;

	let cal: icalendar::Calendar = icalendar::parser::read_calendar(&body)
		.map(Into::into)
		.map_err(|err| anyhow::format_err!("failed to parse calendar: {err}"))?;

	let events: Vec<ui::Event> = cal.components.iter()
		.filter_map(|c| c.as_event())
		.filter_map(|ev| ev.try_into().ok())
		.collect();

	let tasks: Vec<ui::Event> = cal.components.iter()
		.filter_map(|c| c.as_todo())
		.filter_map(|t| t.try_into().ok())
		.collect();

	Ok(Calendar { events, tasks })
}
