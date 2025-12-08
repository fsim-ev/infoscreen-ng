use std::{
	cmp::Ordering, ops::Deref, path::PathBuf, str::FromStr, sync::{Arc, OnceLock}, thread, time
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
use hex_color::HexColor;

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

	show_completed_tasks: u16,
	calendar: Vec<CalendarOpt>,
}

impl Default for Config {
	fn default() -> Self {
		Self {
			update_rate: 120,
			show_completed_tasks: 60,
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

	let time_ticker = slint::Timer::default();
	time_ticker.start(slint::TimerMode::Repeated, time::Duration::from_millis(200), {
		let ui = ui.clone_strong();
		move || {
			//log::debug!("ts: {}", Utc::now().timestamp());
			ui.global::<ui::TimeState>().set_time(Utc::now().timestamp() as _)
		}
	});

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

	ui.upgrade_in_event_loop(move |ui| {
		fn sec_to_dt(sec: i32) -> Option<DateTime<Local>> {
			chrono::Utc.timestamp_opt(sec as _, 0)
				.single()
				.map(|dt| dt.with_timezone(&Local))
		}

		let state = ui.global::<ui::TimeState>();
		state.on_fmt_float(|f, i| format!("{0:.1$}", f, i as _).into());
		state.on_fmt_time(|sec|
			sec_to_dt(sec)
				.map(|dt| dt.format_localized("%H:%M", locale()).to_string())
				.unwrap_or_default()
				.into()
		);
		state.on_fmt_date(|sec|
			sec_to_dt(sec)
				.map(|dt| {
					let dt = dt.date_naive();
					// Remind about new year
					let date_fmt = if dt.month() == 1 { "%A, %e. %B %Y" } else { "%A, %e. %B" };
					dt.format_localized(date_fmt, locale()).to_string()
				})
				.unwrap_or_default()
				.into()
		);
		state.on_is_midnight(|sec|
			sec_to_dt(sec)
				.map(|dt| dt.num_seconds_from_midnight() == 0)
				.unwrap_or_default()
		);

		let state = ui.global::<ui::State>();
		state.on_fmt_time_event(|sec|
			sec_to_dt(sec)
				.map(|dt| dt.format_localized("%H:%M", locale()).to_string())
				.unwrap_or_default()
				.into()
		);
		state.on_fmt_date_event(|sec|
			sec_to_dt(sec)
				.map(|dt| dt.format_localized("%d.%m", locale()).to_string())
				.unwrap_or_default()
				.into()
		);
	}).unwrap();

	let (err_chan, mut _err_rx) = sync::mpsc::channel::<anyhow::Result<()>>(8);
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
			let mut sleep_dur = time::Duration::ZERO;

			loop {
				if !sleep_dur.is_zero() {
					let now = Local::now();
					let dur = Duration::from_std(sleep_dur).unwrap();
					log::info!("next update at {} (in {} min)", (now + dur).to_rfc2822(), dur.num_minutes());
					ui.upgrade_in_event_loop(|ui| ui.set_loading(false))
						.inspect_err(log_err).ok();
				}		
				time::sleep(sleep_dur).await;
				sleep_dur = time::Duration::from_secs(conf.update_rate as _);
				log::info!("updating calendar...");

				let now = chrono::Local::now();
				let mut events = Vec::new();
				let mut tasks = Vec::new();

				for calopt in calopts.iter() {
					log::info!(calendar=calopt.name, "updating");
					let cal = match fetch_calendar(&client, calopt.url.parse().unwrap(), &calopt.auth).await {
						Ok(v) => v,
						Err(err) => {
							log::error!("failed to load calendar: {err}");
							continue;
						}
					};

					log::info!(calendar=calopt.name, event_num=cal.events.len(), task_num=cal.tasks.len(), "loaded");
			
					let deadline = (now.date_naive() - Duration::try_days(1).unwrap())
						.and_time(Default::default())
						.and_local_timezone(Local)
						.single()
						.unwrap();

					log::info!(calendar=calopt.name, "filtering everything before: {}", deadline);
					events.extend(cal.events.into_iter()
						.filter(|e| e.time_end > deadline)
					);
					tasks.extend(cal.tasks.into_iter()
						.filter(|e| e.completed
							.map(|dt| dt > now + Duration::minutes(conf.show_completed_tasks as _))
							.unwrap_or(true)
						)
					);
				}

				events.sort_by(|a, b| a.time_start.cmp(&b.time_start));
				tasks.sort_by(|a, b| {
					fn cmp<T: Ord>(a: Option<T>, b: Option<T>) -> Ordering {
						match (a, b) {
							(Some(at), Some(bt)) => at.cmp(&bt),
							(Some(_), None) => Ordering::Less,
							_ => Ordering::Greater,
						}
					}					
					cmp(a.time_due, b.time_due)
						.then_with(|| cmp(a.priority, b.priority))
						.then_with(|| cmp(a.time_start, b.time_start))
						//.then_with(|| a.time_updated.cmp(&b.time_updated))
				});

				let events_ui: Vec<ui::Event> = events.iter()
					.map(|e| e.into())
					//.inspect(|e: &ui::Event| { log::debug!("{} - {} => {}/100 :: {}", 
					//	e.time_start, e.time_end, 
					//	(now.timestamp() as i32 - e.time_start) as f64/(e.time_end - e.time_start) as f64, &e.summary); })
					.collect();

				let tasks_ui: Vec<ui::Task> = tasks.iter()
					.map(|e| e.into())
					.collect();

				log::debug!(event_num=events.len(), task_num=tasks.len(), "updating UI... ");
				ui.upgrade_in_event_loop(move |ui| {
					fn set_list<T: 'static>(mrc: ModelRc<T>, list: Vec<T>) {
						let model = mrc.as_any()
							.downcast_ref::<VecModel<T>>()
							.expect("unexpected ui model");

						model.set_vec(list);
					}
					set_list(ui.get_events(), events_ui);
					set_list(ui.get_tasks(), tasks_ui);
				}).inspect_err(log_err).ok();
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
		_ = run_token.cancelled() => {},
	}

	log::debug!("IO task shutting down...");
	Ok(())
}

fn log_err<E>(err: &E)
where
	E: std::fmt::Display
{
	log::warn!("failed to update UI: {err}");
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

#[derive(Debug, Clone)]
struct Calendar {
	events: Vec<Event>,
	tasks: Vec<Task>,
}

#[derive(Debug, Clone)]
struct Event {
	summary: String,
	organizer: String,
	location: String,
	color: Color,

	time_start: DateTime<Local>,
	time_end: DateTime<Local>,

	categories: Vec<String>,
}

impl TryFrom<icalendar::Event> for Event {
	type Error = anyhow::Error;

	fn try_from(value: icalendar::Event) -> Result<Self> {
		let event = Event {
			summary: value.get_summary().context("summary missing")?.into(),
			organizer: value.property_value("X-WR-CALNAME").unwrap_or_default().to_owned(),
			color: Color::from_argb_encoded(
				value.property_value("X-APPLE-CALENDAR-COLOR")
					.and_then(|s| HexColor::parse(s).ok())
					.unwrap_or(HexColor::WHITE)
					.to_u32()
			),

			location: value.get_location().unwrap_or_default().into(),
			categories: value.property_value("CATEGORIES")
				.map(|s| s.split(',').map(|s| s.to_string()).collect())
				.unwrap_or_default(),
			time_start: value.get_start()
				.and_then(dt_convert)
				.context("missing start time")?,
			time_end: value.get_end()
				.and_then(dt_convert)
				.context("missing end time")?,
		};
		Ok(event)
	}

}

impl Into<ui::Event> for &Event {
	fn into(self) -> ui::Event {
		ui::Event {
			summary: self.summary.clone().into(),
			location: self.location.clone().into(),
			organizer: self.organizer.clone().into(),
			color: self.color,

			categories: self.categories.join(", ").into(),
			time_start: self.time_start.timestamp() as _,
			time_end: self.time_end.timestamp() as _,
		}
	}
}

#[derive(Debug, Clone)]
struct Task {
	summary: String,
	organizer: String,
	color: Color,
	
	//time_updated: DateTime<Local>,
	time_start: Option<DateTime<Local>>,
	time_due: Option<DateTime<Local>>,
	priority: Option<u32>,

	completed: Option<DateTime<Local>>,
	progress: u8,
}

impl TryFrom<icalendar::Todo> for Task {
	type Error = anyhow::Error;

	fn try_from(todo: icalendar::Todo) -> Result<Task> {
		let task = Task {
			summary: todo.get_summary().context("summary missing")?.into(),
			organizer: Default::default(),
			color: Default::default(),

			//time_updated: todo.get_timestamp()
			//	.context("task has no update date")?
			//	.with_timezone(&Local),
			time_start: todo.get_start().and_then(dt_convert),
			time_due: todo.get_due().and_then(dt_convert),
			priority: todo.get_priority(),

			completed: todo.get_completed()
				.or_else(||
					// Parsing this ourselves, as NextCloud fails to provide comliant dates
					todo.property_value("COMPLETED")
						.and_then(|naive_dt_str|
							NaiveDateTime::parse_from_str(naive_dt_str, "%Y%m%dT%H%M%S")
								.context("failed to parse COMPLETED date")
								.and_then(|dt| dt.and_local_timezone(Local)
									.single()
									.context("failed to set local time zone")
								)
								.inspect_err(|err|
									log::warn!("failed to parse COMPLETED date '{:?}': {}", naive_dt_str, err)
								)
								.map(|dt| dt.to_utc())
								.ok()
						)
				)
				.map(|dt| dt.with_timezone(&Local)),
			progress: todo.get_percent_complete()
				.unwrap_or_default(),
		};
		Ok(task)
	}
}

impl Into<ui::Task> for &Task {
	fn into(self) -> ui::Task {
		ui::Task {
			summary: self.summary.clone().into(),
			organizer: self.organizer.clone().into(),
			color: self.color,

			time_start: self.time_start
				.map(|dt| dt.timestamp() as _)
				.unwrap_or_default(),
			time_due: self.time_due
				.map(|dt| dt.timestamp() as _)
				.unwrap_or_default(),
			priority: self.priority
				.map(|p| p as _)
				.unwrap_or(std::i32::MAX),

			complete: self.completed.is_some(),
			progress: self.progress as _,
		}
	}
}


fn dt_convert(pdt: DatePerhapsTime) -> Option<DateTime<Local>> {
	match pdt {
		DatePerhapsTime::DateTime(cdt) => 
			if let icalendar::CalendarDateTime::Floating(ndt) = cdt {
				ndt.and_local_timezone(Local)
					.single()
					.or_else(|| {
						log::warn!("failed to convert date to datetime: {:?}", ndt);
						None
					})
			} else {
				cdt.try_into_utc()
					.or_else(|| {
						log::warn!("failed to convert datetime to UTC: {:?}", cdt);
						None
					})
					.map(|cdt| cdt.with_timezone(&Local))
			},
		DatePerhapsTime::Date(nd) => 
			nd.and_time(chrono::NaiveTime::from_hms_opt(0, 0, 0).unwrap())
				.and_local_timezone(chrono::Local)
				.single()
				.or_else(|| {
					log::warn!("failed to convert date to datetime: {:?}", nd);
					None
				})
	}
}


async fn fetch_calendar(client: &http::Client, url: http::Url, auth: &Option<Auth>) -> Result<Calendar> 
{
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

	let organizer = cal.property_value("X-WR-CALNAME")
		.unwrap_or_default()
		.trim_end_matches("(FSIM)") // HACK!
		.trim()
		.to_owned();
	let color = {
		let (r,g,b) = cal.property_value("X-APPLE-CALENDAR-COLOR")
			.and_then(|s| 
				HexColor::parse(s)
					.inspect_err(|e| log::warn!("failed to parse color '{s}': {e}"))
					.ok()
			)
			.unwrap_or(HexColor::WHITE)
			.split_rgb();
		 Color::from_rgb_u8(r,g,b)
	};

	log::debug!(organizer=organizer, "loaded calendar: color: {:8X}", color.as_argb_encoded());

	let events = cal.components.iter()
		.filter_map(|c| c.as_event())
		.cloned()
		.filter_map(|e| e.try_into()
			.inspect_err(|err| log::warn!(organizer=organizer, "failed to process task: {err}"))
			.ok()
		)
		.map(|e| Event {
			organizer: organizer.clone(),
			color,
			..e
		})
		.collect();

	let tasks = cal.components.iter()
		.filter_map(|c| c.as_todo())
		.cloned()
		.filter_map(|t| t.try_into()
			.inspect_err(|err| log::warn!(organizer=organizer, "failed to process task: {err}"))
			.ok()
		)
		.map(|e| Task {
			organizer: organizer.clone(),
			color,
			..e
		})
		.collect();

	Ok(Calendar { events, tasks })
}
