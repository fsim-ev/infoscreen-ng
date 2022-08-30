use std::{
	collections::{HashMap, HashSet},
	hash::Hash,
	ops::Add,
	path::PathBuf,
	sync::{Arc, Once},
	thread, time,
};

use clap::Parser;
use slint::{self, Color, ComponentHandle, VecModel, Weak, SharedString};

use reqwest as http;
use tokio;
use tokio_util::sync::CancellationToken;

use chrono::{prelude::*, Duration};
use figment::{
	providers::{Format, Serialized, Toml},
	Figment,
};
use serde::{Deserialize, Deserializer, Serialize};
use xdg;

use anyhow::{Context, Result};
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

#[derive(Debug, Default, Deserialize, Serialize)]
struct Config {
	update_rate: u32,
	strip_from_names: String,
	walk_speed: WalkSpeed,
	bus_stops: HashMap<String, BusStop>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct WalkSpeed {
	normal: f32,
	fast: f32,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct BusStop {
	title: String,
	color: u32,
	#[serde(default)]
	distance: u32, // in m

	#[serde(default)]
	stops_into_city: HashMap<String, HashSet<String>>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
	// Config init
	let opts = Options::parse();
	let xdg_dirs = xdg::BaseDirectories::with_prefix("infoscreen")?;

	let fig = Figment::new()
		.merge(Serialized::defaults(Config::default()))
		.merge(Toml::file(&xdg_dirs.get_config_file("bus-schedule.toml")))
		.merge(Toml::file(&opts.config_path))
		.merge(Serialized::defaults(opts));

	let conf: Config = fig.extract()?;

	// Logging system init
	tracing_subscriber::fmt()
		.with_env_filter(
			tracing_subscriber::EnvFilter::from_default_env()
				.add_directive(tracing_subscriber::filter::LevelFilter::INFO.into())
				.add_directive("infoscreen_bus_schedule=debug".parse()?)
				.add_directive("reqwest=debug".parse()?), //.add_directive("hyper=trace".parse()?)
		)
		.compact()
		.init();

	fig.metadata().for_each(|md| {
		if let Some(src) = md.source.as_ref() {
			log::debug!("using config from {} - {}", md.name, src);
		}
	});

	// Check config
	if conf.bus_stops.is_empty() {
		return Err("empty bus schedule".into());
	}

	let ui = ui::App::new();
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
					ui.set_status(err_str.into());
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

	ui.run();
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

	let time_task = spawn({
		let ui = ui.clone();
		async move {
			let mut old_date = chrono::Local.timestamp(0, 0).date();
			let mut old_time = chrono::Local.timestamp(0, 0).time();
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
				let new_date = if now.date() != old_date {
					let date = now.date();
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
					if let Some(date) = new_date {
						ui.set_date(date.into());
					}
					if let Some(time) = new_time {
						ui.set_time(time.into());
						ui.global::<ui::DepartureState>()
							.set_time((now.timestamp() / 60) as _);
					}
					ui.set_secs(now.second() as i32);
				})
				.ok();
				time::sleep(time::Duration::from_millis(250)).await;
			}
		}
	});

	let (err_chan, mut err_rx) = sync::mpsc::channel::<anyhow::Result<()>>(8);
	spawn({
		let ui = ui.clone();
		async move {
			while let Some(res) = err_rx.recv().await {
				let status: SharedString = match res {
					Ok(()) => "".into(),
					Err(err) => {
						let cause: String = err
							.chain()
							.skip(1)
							.take(1)
							.map(|err| err.to_string())
							.collect::<String>()
							.split(": ")
							.collect::<Vec<_>>()
							.join("\n");
						format!("{}\n{}", err, cause).into()
					}
				};
				ui.upgrade_in_event_loop(|ui| {
					ui.set_status(status);
				})
				.ok();
			}
		}
	});

	let mut bus_stops: Vec<_> = conf.bus_stops.into_iter().collect();
	bus_stops.sort_by(|a, b| a.1.distance.cmp(&b.1.distance));

	let bus_stops = Arc::new(bus_stops);
	ui.upgrade_in_event_loop({
		let bus_stops = bus_stops.clone();
		move |ui| {
			let ui_stops: Vec<_> = bus_stops
				.clone()
				.iter()
				.map(|(_, stop)| {
					let dist = stop.distance;
					ui::BusStop {
						name: stop.title.clone().into(),
						color: Color::from_argb_encoded(stop.color),
						distance: dist as _,
						time_min: (dist as f32 / (conf.walk_speed.fast * 1.0 / 3.6) / 60.0).ceil()
							as _,
						time_max: (dist as f32 / (conf.walk_speed.normal * 1.0 / 3.6) / 60.0).ceil()
							as _,
					}
				})
				.collect();
			ui.global::<ui::DepartureState>()
				.set_local_stop_list(VecModel::from_slice(&ui_stops))
		}
	})
	.ok();

	let client = http::Client::builder()
		.connect_timeout(time::Duration::from_secs(10))
		.timeout(time::Duration::from_secs(20))
		.pool_idle_timeout(time::Duration::from_secs((conf.update_rate + 2) as _))
		.build()?;
	let schedule_updater = spawn({
		let ui = ui.clone();
		let err_chan = err_chan.clone();
		let bus_stops = bus_stops.clone();
		let strip = conf.strip_from_names;
		async move {
			let mut departures: HashMap<&str, Vec<Departure>> =
				HashMap::with_capacity(bus_stops.len());

			loop {
				log::info!("updating bus schedules...");

				let now = chrono::Local::now();
				let mut fetch_err = false;

				for (stop_idx, (stop_id, stop)) in bus_stops.iter().enumerate() {
					log::debug!(stop = stop_id, "  fetching schedule...");
					let mut deps_new = match fetch_schedule(client.clone(), stop_id).await {
						Ok(v) => v,
						Err(err) => {
							log::error!(%err, "failed to fetch schedule");
							fetch_err = true;
							err_chan.try_send(Err(err)).ok();
							continue;
						}
					};
					log::debug!(stop = stop_id, "  loaded {} departures", deps_new.len());
					if deps_new.is_empty() {
						continue;
					}

					for dep in &mut deps_new {
						dep.local_stop_idx = stop_idx;
						if let Some(stops_into_city) = stop.stops_into_city.get(&dep.line) {
							dep.into_city = stops_into_city.contains(&dep.line_destination);
						}
						if let Some(stripped) = dep.line_destination.strip_prefix(&strip) {
							dep.line_destination = stripped.trim().to_owned();
						}
					}

					let deps_old = departures.entry(&stop_id).or_default();
					*deps_old = deps_old.iter()
						.cloned()
						.filter(|d| {
							let dtime = d.time + Duration::minutes(d.delay as _);
							dtime < deps_new.first().unwrap().time && dtime > now - Duration::minutes(2)
						})
						.collect();

					deps_old.extend(deps_new);
				}

				log::debug!("updating UI...");

				let mut departures: Vec<_> = departures.values().flatten().collect();
				let dep_num = departures.len();
				departures.sort_by(|a, b| {
					a.time
						.add(Duration::minutes(a.delay as _))
						.cmp(&b.time.add(Duration::minutes(b.delay as _)))
				});

				let (depar_into_city, depar_from_city) = {
					departures
						.into_iter()
						.partition::<Vec<_>, _>(|depart| depart.into_city)
				};

				ui.upgrade_in_event_loop(|ui| {
					let dep: Vec<_> = depar_into_city.into_iter().map(Into::into).collect();
					ui.set_schedule_into_city(VecModel::from_slice(&dep));
					let dep: Vec<_> = depar_from_city.into_iter().map(Into::into).collect();
					ui.set_schedule_from_city(VecModel::from_slice(&dep));
				})
				.ok();
				log::info!("updated {dep_num} departures...");

				let delay = if fetch_err {
					time::Duration::from_secs(10)
				} else {
					err_chan.try_send(Ok(())).ok();
					time::Duration::from_secs(conf.update_rate as _)
				};
				time::sleep(delay).await;
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
	static LOCALE_INIT: Once = Once::new();
	static mut LOCALE: chrono::Locale = chrono::Locale::POSIX;

	unsafe {
		LOCALE_INIT.call_once(|| {
			match sys_locale::get_locale()
				.and_then(|locstr| chrono::Locale::try_from(locstr.as_str()).ok())
			{
				Some(loc) => LOCALE = loc,
				None => log::error!("failed to get system locale"),
			}
		});
		LOCALE
	}
}

#[derive(Clone, Debug, Default)]
struct Departure {
	time: DateTime<Local>,
	delay: i32,

	line: String,
	line_destination: String,
	into_city: bool,

	local_stop_idx: usize,
}

impl Hash for Departure {
	fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
		self.local_stop_idx.hash(state);
		self.time.hash(state);
		self.line.hash(state);
		self.line_destination.hash(state);
	}
}

impl PartialEq for Departure {
	fn eq(&self, other: &Self) -> bool {
		self.local_stop_idx == other.local_stop_idx
			&& self.time == other.time
			&& self.line == other.line
			&& self.line_destination == other.line_destination
	}
}
impl Eq for Departure {}

impl Into<ui::Departure> for Departure {
	fn into(self) -> ui::Departure {
		ui::Departure {
			time: self.time.format("%H:%M").to_string().into(),
			delay: self.delay,
			deadline: (self.time.timestamp() / 60) as i32 + self.delay,

			line: self.line.into(),
			final_stop: self.line_destination.into(),
			local_stop_idx: self.local_stop_idx as _,
		}
	}
}

impl<'de> Deserialize<'de> for Departure {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: Deserializer<'de>,
	{
		#[derive(Deserialize)]
		struct Stop {
			#[serde(rename = "dateTime")]
			time_sched: Time,

			#[serde(rename = "realDateTime")]
			time_real: Option<Time>,

			#[serde(rename = "servingLine")]
			line: ServingLine,
		}
		#[derive(Deserialize)]
		struct Time {
			#[serde(deserialize_with = "parse_u32")]
			year: u32,
			#[serde(deserialize_with = "parse_u32")]
			month: u32,
			#[serde(deserialize_with = "parse_u32")]
			day: u32,
			#[serde(deserialize_with = "parse_u32")]
			hour: u32,
			#[serde(deserialize_with = "parse_u32")]
			minute: u32,
		}
		fn parse_u32<'de, D>(deserializer: D) -> Result<u32, D::Error>
		where
			D: Deserializer<'de>,
		{
			let s = String::deserialize(deserializer)?;
			let i = u32::from_str_radix(&s, 10).map_err(serde::de::Error::custom)?;
			Ok(i)
		}

		#[derive(Deserialize)]
		struct ServingLine {
			symbol: String,
			direction: String,

			#[serde(rename = "liErgRiProj")]
			proj: LiErgRiProj,
		}

		#[derive(Deserialize)]
		struct LiErgRiProj {
			direction: String,
		}

		let stop = Stop::deserialize(deserializer)?;
		let time_sched = {
			let ts = stop.time_sched;
			chrono::Local
				.ymd(ts.year as _, ts.month, ts.day)
				.and_hms(ts.hour, ts.minute, 0)
		};
		let delay = stop
			.time_real
			.map(|ts| {
				chrono::Local
					.ymd(ts.year as _, ts.month, ts.day)
					.and_hms(ts.hour, ts.minute, 0)
			})
			.map(|ts_real| (ts_real - time_sched).num_minutes())
			.unwrap_or_default();

		let into_city = stop.line.proj.direction == "R" || stop.line.direction.contains("Hbf"); // HACK: remove me

		Ok(Departure {
			time: time_sched,
			delay: delay as i32,
			line: stop.line.symbol,
			line_destination: stop.line.direction,
			into_city,
			..Default::default()
		})
	}
}

async fn fetch_schedule(client: http::Client, stop: &str) -> Result<Vec<Departure>> {
	#[derive(Debug, Deserialize)]
	struct Response {
		#[serde(rename = "departureList")]
		list: Vec<Departure>,
	}

	client
		.get("https://mobile.defas-fgi.de/beg/json/XML_DM_REQUEST")
		.query(&[
			("outputFormat", "JSON"),
			("language", "de"),
			("stateless", "1"),
			("type_dm", "stop"),
			("mode", "direct"),
			("useRealtime", "1"),
			("ptOptionActive", "1"),
			("mergeDep", "1"),
			("deleteAssignedStops_dm", "1"),
			("limit", "60"),
			("name_dm", stop),
		])
		.send()
		.await
		.and_then(|res| res.error_for_status())
		.context("failed to fetch data")?
		.json::<Response>()
		.await
		.map(|res| res.list)
		.context("failed to parse data")
}
