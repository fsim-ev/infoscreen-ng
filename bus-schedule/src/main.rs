use std::{
	collections::{HashMap, HashSet, BTreeSet},
	hash::Hash,
	ops::Deref,
	path::PathBuf,
	sync::{Arc, Once},
	thread, time,
};

use clap::Parser;
use slint::{self, 
	ComponentHandle, Weak,
	Model,
	Color, VecModel, SharedString, ModelRc, 
};

use reqwest as http;
use tokio;
use tokio_util::sync::CancellationToken;

use chrono::prelude::*;
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

	let ui = ui::App::new()?;

	let time_ticker = slint::Timer::default();
	time_ticker.start(slint::TimerMode::Repeated, time::Duration::from_millis(200), {
		let ui = ui.clone_strong();
		move || ui.global::<ui::TimeState>().set_time(Utc::now().timestamp() as _)
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

	if let Err(err) = slint::run_event_loop() {
		log::error!(err=%err, "failed to start UI task...");
	} else {
		log::debug!("UI task shutting down...");
	}
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

async fn io_run(ui: Weak<ui::App>, conf: Config, run_token: CancellationToken) -> Result<()> 
{
	use tokio::*;

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
	}).unwrap();

	let time_task = spawn({
		let ui = ui.clone();
		async move {
			loop {
				let now = chrono::Local::now();
				
				ui.upgrade_in_event_loop(move |ui| {
					ui.global::<ui::DepartureState>()
						.set_time((now.timestamp() / 60) as _);

				}).inspect_err(log_err).ok();
				time::sleep(time::Duration::from_secs(10)).await;
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
						let cause: String = err.chain()
							.skip(1)
							.take(1)
							.map(ToString::to_string)
							.collect::<String>()
							.split(": ")
							.collect::<Vec<_>>()
							.join("\n");

						format!("{}\n{}", err, cause).into()
					}
				};
				ui.upgrade_in_event_loop(|ui| {
					ui.set_status(status);
				}).inspect_err(log_err).ok();
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
						time_min: (dist as f32 / (conf.walk_speed.fast * 1.0 / 3.6) / 60.0).ceil() as _,
						time_max: (dist as f32 / (conf.walk_speed.normal * 1.0 / 3.6) / 60.0).ceil() as _,
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

	let (tx, mut rx) = sync::mpsc::channel(16);

	let schedule_updater = spawn({
		let err_chan = err_chan.clone();
		let bus_stops = bus_stops.clone();
		let strip = conf.strip_from_names;
		async move {
			loop {
				let mut fetch_err = false;
				let mut dep_next = Local::now();

				log::info!("updating bus schedules...");
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

						if dep_next > dep.time_sched {
							dep_next = dep.time_sched;
						}
					}

					let deps_new: BTreeSet<_> = deps_new.into_iter().collect();

					if let Err(err) = tx.send(deps_new).await {
						log::error!("failed to dispatch departures: {}", err);
						continue;
					}
				}


				let delay = if fetch_err {
					time::Duration::from_secs(10)
				} else {
					err_chan.try_send(Ok(())).ok(); // clear error

					let threshold = chrono::Duration::minutes((conf.update_rate * 20) as _);
					let wait = Local::now() - dep_next;
					if wait > threshold {
						// slow down updating if the next departures
						time::Duration::from_secs((wait.num_seconds() / 2) as _)
					} else {
						time::Duration::from_secs(conf.update_rate as _)
					}
				};
				time::sleep(delay).await;
			}
		}
	});
	let _ui_updater = spawn({
		let ui = ui.clone();
		let bus_stops = bus_stops.clone();
		let _err_chan = err_chan.clone();
		async move {

			ui.upgrade_in_event_loop(move |ui| {
				// prime type id for later
				ui.set_schedule_into_city(Vec::<ui::Departure>::default().deref().into());
				ui.set_schedule_from_city(Vec::<ui::Departure>::default().deref().into());
			}).ok();

			let mut line_dist: HashMap<String, (usize, u32)> = Default::default();
			let mut entries: BTreeSet<Departure> = Default::default();
			let sleep_dur = time::Duration::from_secs(60);

			loop {
				match time::timeout(sleep_dur, rx.recv()).await {
					Ok(Some(new_entries)) => {
						for entry in new_entries {
							let stop = bus_stops.get(entry.local_stop_idx)
								.map(|(_id, stop)| stop)
								.unwrap();

							line_dist.entry(entry.line.clone())
								.and_modify(|(idx, dist)| {
									if stop.distance < *dist {
										*idx = entry.local_stop_idx;
										*dist = stop.distance;
									}
								})
								.or_insert((entry.local_stop_idx, stop.distance));
													
							entries.replace(entry);
						}
					},
					Ok(None) => break,
					Err(_) => {},
				}
				log::debug!("updating UI...");

				let now = Local::now();
				entries.retain(|e| e.time_real > now
					&& line_dist.get(&e.line)
						.map(|(i,_)| *i == e.local_stop_idx)
						.unwrap_or(true)
				);

				let mut entries: Vec<_> = entries.iter().collect();
				entries.sort_by(|a, b| {
					a.time_real.cmp(&b.time_real)
						.then_with(||
							bus_stops.get(a.local_stop_idx)
								.zip(bus_stops.get(b.local_stop_idx))
								.map(|(a, b)| (a.1.distance, b.1.distance))
								.map(|(a, b)| a.cmp(&b))
								.unwrap()
						)
				});

				let num = entries.len();
				let (depar_into_city, depar_from_city) = entries.into_iter()
					.cloned()
					.partition::<Vec<_>, _>(|depart| depart.into_city);

				ui.upgrade_in_event_loop(|ui| {
						fn set_departures(mrc: ModelRc<ui::Departure>, deps: Vec<Departure>) {
							let model = mrc.as_any()
								.downcast_ref::<VecModel<ui::Departure>>()
								.expect("unexpected ui model");

							let deps: Vec<ui::Departure> = deps.into_iter().map(Into::into).collect(); 
							model.set_vec(deps);
						}

						set_departures(ui.get_schedule_into_city(), depar_into_city);
						set_departures(ui.get_schedule_from_city(), depar_from_city);
				}).inspect_err(log_err).ok();
				
				log::info!("updated {num} departures...");
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

fn log_err<E>(err: &E)
where
	E: std::fmt::Display
{
	log::warn!("failed to update UI: {err}");
}

fn locale() -> chrono::Locale {
	static LOCALE_INIT: Once = Once::new();
	static mut LOCALE: chrono::Locale = chrono::Locale::POSIX;

	unsafe {
		LOCALE_INIT.call_once(|| {
			match sys_locale::get_locale()
				.ok_or("detect system locale".to_owned())
				.and_then(|locstr| chrono::Locale::try_from(locstr.replace('-', "_").as_str())
					.map_err(|_err| format!("parse system locale '{locstr}'"))
				)
			{
				Ok(loc) => LOCALE = loc,
				Err(err) => log::error!("failed to {err}"),
			}
		});
		LOCALE
	}
}

#[derive(Clone, Debug, Default)]
struct Departure {
	time_sched: DateTime<Local>,
	time_real: DateTime<Local>,

	line: String,
	line_destination: String,
	into_city: bool,

	local_stop_idx: usize,
}

impl Hash for Departure {
	fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
		self.local_stop_idx.hash(state);
		self.time_sched.hash(state);
		self.line.hash(state);
		self.line_destination.hash(state);
	}
}

impl PartialEq for Departure {
	fn eq(&self, other: &Self) -> bool {
		self.local_stop_idx == other.local_stop_idx
			&& self.time_sched == other.time_sched
			&& self.line == other.line
			&& self.line_destination == other.line_destination
	}
}
impl Eq for Departure {}

impl PartialOrd for Departure {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Departure {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.line.cmp(&other.line)
        	.then_with(|| self.line_destination.cmp(&other.line_destination))
        	.then_with(|| self.time_sched.cmp(&other.time_sched))
    }
}

impl Into<ui::Departure> for Departure {
	fn into(self) -> ui::Departure {
		ui::Departure {
			time: self.time_sched.format("%H:%M").to_string().into(),
			delay: (self.time_real - self.time_sched).num_minutes() as _,
			deadline: (self.time_real.timestamp() / 60) as _,

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
			chrono::Local.with_ymd_and_hms(ts.year as _, ts.month, ts.day, ts.hour, ts.minute, 0)
				.latest()
				.ok_or(serde::de::Error::custom("failed to parse scheduled time"))?
		};
		let time_real = stop.time_real
			.and_then(|ts|
				chrono::Local.with_ymd_and_hms(ts.year as _, ts.month, ts.day, ts.hour, ts.minute, 0)
					.latest()
			)
			.unwrap_or(time_sched);

		let into_city = stop.line.proj.direction == "R" || stop.line.direction.contains("Hbf"); // HACK: remove me

		Ok(Departure {
			time_sched,
			time_real,
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
