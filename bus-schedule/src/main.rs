use std::{
	sync::Once,
	ops::Add,
	thread, time,
};

use clap::Parser;
use slint::{self,
	ComponentHandle, Weak,
	Color, VecModel,
	SharedString
};

use tokio;
//use futures_util::{TryFutureExt, FutureExt, TryStreamExt, StreamExt};
use reqwest as http;

use serde::{Deserialize, Deserializer};
use chrono::{
	prelude::*,
	Duration,
};

use anyhow::{Result, Context};
use sys_locale;

use tracing as log;
use tracing_subscriber;

mod ui {
	slint::include_modules!();
}

#[derive(Parser, Debug)]
#[clap(about, version)]
struct Opt
{
	// Some options may arrive here later...
}


fn main() -> Result<(), Box<dyn std::error::Error>>
{
	let opt = Opt::parse();

	tracing_subscriber::fmt()
		.with_env_filter(tracing_subscriber::EnvFilter::from_default_env()
			.add_directive("reqwest=debug".parse()?)
			//.add_directive("hyper=debug".parse()?)
		)		
		.with_max_level(log::Level::DEBUG)
		.compact()
		.init();

	let ui = ui::App::new();
	let io_task_handle = thread::Builder::new()
		.name("io-runtime".into())
		.spawn({
			let ui = ui.as_weak();
			move || io_runtime_run(ui.clone(), opt).expect("fatal error")
		})?;

	let _cleanup_task_handle = thread::Builder::new()
		.name("cleanup".into())
		.spawn({
			let ui = ui.as_weak();
			move || {
				if let Err(panic) = io_task_handle.join()
				{
					let err_str = panic.downcast::<String>()
						.map(|str_box| *str_box)
						.or_else(|panic| panic.downcast::<&str>().map(|str_box| str_box.to_string()))
						.unwrap_or_else(|panic| format!("unknown panic type: {:?}", panic));

					log::error!("PANIC! {}", err_str);
					thread::sleep(time::Duration::from_secs(1));
					ui.upgrade_in_event_loop(move |ui| {
						ui.set_status(err_str.into());
					});
					thread::sleep(time::Duration::from_secs(10));
				}
				slint::invoke_from_event_loop(move || slint::quit_event_loop());
			}
		});

	ui.run();
	Ok(())
}

fn io_runtime_run(ui: Weak<ui::App>, opt: Opt) -> Result<()>
{
	use tokio::*;

	let rt = runtime::Builder::new_current_thread()
		.enable_all()
		.build()?;

	rt.block_on(io_run(ui, opt))?;
	rt.shutdown_timeout(time::Duration::from_secs(10));

	Ok(())
}

async fn io_run(ui: Weak<ui::App>, _opt: Opt) -> Result<()>
{
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
					let date_fmt = if date.month() == 1 { "%A, %e. %B %Y" } else { "%A, %e. %B" };
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
						ui.global::<ui::DepartureState>().set_time((now.timestamp() / 60) as _);
					}
					ui.set_secs(now.second() as i32);
				});
				time::sleep(time::Duration::from_millis(250)).await;
			}
		}
	});

	let client = http::Client::new();
	spawn({
		let ui = ui.clone();
		let client = client.clone();
		async move {
			loop {
				let strip = "Regensburg ";
				let local_stops: std::collections::HashMap<_,_> = [
						("TechCampus/OTH", ("TechCampus",  0x44_4A78D3)),
						("OTH Regensburg", ("OTH", 0x44_006633)),
					].into_iter()
					.collect();

				let mut errors = Vec::new();
				let mut departures = Vec::new();
				for &stop in local_stops.keys() {
					let departs = match fetch_schedule(client.clone(), stop.into()).await {
						Ok(v) => v,
						Err(err) => {
							log::error!(%err, "failed to fetch schedule");
							errors.push(err);
							continue;
						},
					};
					departures.extend(departs.into_iter()
						.map(|depart| {
							let desc = local_stops.get(&*depart.stop_local).unwrap();
							Departure {
								line_destination: match depart.line_destination.strip_prefix(strip) {
										Some(stripped) => stripped.to_owned(),
										None => depart.line_destination,
									},
								stop_local: desc.0.to_string(),
								shade: Color::from_argb_encoded(desc.1),
								..depart
						}})
					);
				}

				departures.sort_by(|a,b|
					a.time.add(Duration::minutes(a.delay as _))
						.cmp(&b.time.add(Duration::minutes(b.delay as _)))
				);

				let (depar_into_city, depar_from_city) = departures.into_iter()
					.partition::<Vec<_>, _>(|depart| depart.into_city);

				ui.upgrade_in_event_loop(|ui| {
					if errors.is_empty() {
						ui.set_status(Default::default());
					} else {
						let errs = errors.into_iter()
							.map(|err| error_showable(err))
							.fold(SharedString::default(), |acc, err| acc + &err.to_string());
						ui.set_status(errs);
					}

					let dep: Vec<_> = depar_into_city.into_iter().map(Into::into).collect();
					ui.set_schedule_into_city(VecModel::from_slice(&dep));

					let dep: Vec<_> = depar_from_city.into_iter().map(Into::into).collect();
					ui.set_schedule_from_city(VecModel::from_slice(&dep));					
				});

				time::sleep(time::Duration::from_secs(120)).await;
			}
		}
	});

	time_task.await?;
	Ok(())
}

#[derive(Debug)]
struct Departure {
	time: DateTime<Local>,
	delay: i32,

	line: String,
	line_destination: String,
	into_city: bool,

	stop_local: String,
	shade: Color,
}

impl Into<ui::Departure> for Departure {
	fn into(self) -> ui::Departure {
		ui::Departure {
			time: self.time.format("%H:%M").to_string().into(),
			delay: self.delay,
			deadline: (self.time.timestamp() / 60) as i32 + self.delay,

			line: self.line.into(),
			final_stop: self.line_destination.into(),

			local_stop: self.stop_local.into(),
			shade: self.shade,
		}
	}
}

impl<'de> Deserialize<'de> for Departure {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
		where D: Deserializer<'de>
	{
		#[derive(Deserialize)]
		struct Stop {
			#[serde(rename="dateTime")]
			time_sched: Time,

			#[serde(rename="realDateTime")]
			time_real: Option<Time>,

			#[serde(rename="nameWO")]
			stop_local: String,

			#[serde(rename="servingLine")]
			line: ServingLine,
		}
		#[derive(Deserialize)]
		struct Time {
			#[serde(deserialize_with="parse_u32")]
			year: u32,
			#[serde(deserialize_with="parse_u32")]
			month: u32,
			#[serde(deserialize_with="parse_u32")]
			day: u32,
			#[serde(deserialize_with="parse_u32")]
			hour: u32,
			#[serde(deserialize_with="parse_u32")]
			minute: u32,
		}
		fn parse_u32<'de, D>(deserializer: D) -> Result<u32, D::Error>
			where D: Deserializer<'de>
		{
			let s = String::deserialize(deserializer)?;
			let i = u32::from_str_radix(&s, 10)
				.map_err(serde::de::Error::custom)?;
			Ok(i)
		}

		#[derive(Deserialize)]
		struct ServingLine {
			symbol: String,
			direction: String,

			#[serde(rename="liErgRiProj")]
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
		let delay = stop.time_real
			.map(|ts| chrono::Local
				.ymd(ts.year as _, ts.month, ts.day)
				.and_hms(ts.hour, ts.minute, 0)
			)
			.map(|ts_real| (ts_real - time_sched).num_minutes())
			.unwrap_or_default();

		let into_city = stop.line.proj.direction == "R" ||
			stop.line.direction.contains("Hbf"); // HACK: remove me

		Ok(Departure {
			time: time_sched,
			delay: delay as i32,
			line: stop.line.symbol,
			line_destination: stop.line.direction,
			into_city,
			stop_local: stop.stop_local,
			shade: Color::from_argb_encoded(0),
		})
	}
}

async fn fetch_schedule(client: http::Client, stop: String) -> Result<Vec<Departure>> {
	#[derive(Debug, Deserialize)]
	struct Response {
		#[serde(rename="departureList")]
		list: Vec<Departure>
	}

	client.get("https://mobile.defas-fgi.de/beg/json/XML_DM_REQUEST")
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
		])
		.query(&[("name_dm", stop)])
		.send()
		.await
		.and_then(|res| res.error_for_status())
		.context("failed to fetch data")?
		.json::<Response>()
		.await
		.map(|res| res.list)
		.context("failed to parse data")
}


fn error_showable(err: anyhow::Error) -> SharedString {
	let cause: String = err.chain().skip(1).take(1)
		.map(|err| err.to_string()).collect::<String>()
		.split(": ").collect::<Vec<_>>()
		.join("\n");

	format!("{}\n{}", err, cause).into()
}

fn locale() -> chrono::Locale {
	static LOCALE_INIT: Once = Once::new();
	static mut LOCALE: chrono::Locale = chrono::Locale::POSIX;

	unsafe {
		LOCALE_INIT.call_once(|| {
			LOCALE = sys_locale::get_locale()
				.and_then(|locstr| chrono::Locale::try_from(locstr.as_str()).ok())
				.expect("failed to get system locale");
		});
		LOCALE
	}
}

