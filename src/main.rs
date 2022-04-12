use std::{
	sync::Once,
	thread, time,
};

use clap::Parser;
use slint::{self,
	ComponentHandle, Weak,
	VecModel,
	Color, SharedString,
};

use tokio;
use reqwest as http;
use futures_util::TryFutureExt;

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
	#[clap(default_value = "http://127.0.0.1:5000")]
	/// Base URL of the lecture data source
	url: reqwest::Url,
}


fn main() -> Result<(), Box<dyn std::error::Error>>
{
	let opt = Opt::parse();

	tracing_subscriber::fmt()
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
						ui.set_lectures_status(err_str.into());
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

async fn io_run(ui: Weak<ui::App>, opt: Opt) -> Result<()>
{
	use tokio::*;

	let time_task = spawn({
		let ui = ui.clone();
		async move {
			let mut old_date = Local.timestamp(0, 0).date();
			let mut old_time = Local.timestamp(0, 0).time();
			loop {
				let now = Local::now();
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
						ui.global::<ui::State>()
							.set_day_time((now.hour() * 100 + now.minute()) as _);
					}
					ui.set_secs(now.second() as i32);
				});
				time::sleep(time::Duration::from_millis(250)).await;
			};
	}});
/*
	let client = http::ClientBuilder::new()
		.cookie_store(true)
		.build()
		.context("failed to init http client")?;

	let _lecture_updater: task::JoinHandle<Result<()>> = spawn({
		let ui = ui.clone();
		let client = client.clone();
		let url = opt.url.join("lectures.json").unwrap();
		async move {
			loop {
				log::debug!("Loading lectures from {} ...", &url);
				let response: Result<Vec<LectureOld>> = client.get(url.clone())
					.send()
					.and_then(http::Response::json)
					.await.context("failed to load lectures, reloading in 30s");

				let lectures = match response {
					Ok(v) => v,
					Err(err) => {
						log::error!("{:#}", err);
						ui.clone().upgrade_in_event_loop(move |ui| {
							ui.set_lectures_status(error_showable(err));
						});
						time::sleep(time::Duration::from_secs(30)).await;
						continue;
					},
				};

				log::info!("Loaded {} lectures", lectures.len());
				let now = chrono::Local::now().naive_local();

				let mut time_blocks: std::collections::BTreeMap<_, Vec<Lecture>> = Default::default();
				for mut lecture in lectures
				{
					lecture.locations.sort();
					lecture.courses.sort();
					time_blocks.entry(lecture.time).or_default().push(lecture);
				}
				time_blocks.values_mut().for_each(|lectures|
					lectures.sort_by(|a, b|	a.abbr.cmp(&b.abbr))
				);
				let next_update = time_blocks.iter()
					.map(|(&time, _)| time + Duration::minutes(30) - now)
					.nth(0).unwrap_or(Duration::hours(1));

				ui.upgrade_in_event_loop(move |ui| {
					ui.set_lectures_status(Default::default());
					let now = chrono::Local::now();
					let mut dated = None;
					let ui_blocks: Vec<ui::TimeBlock> = time_blocks.into_iter()
						.map(|(time, lectures)|{
							let time = chrono::Local.from_local_datetime(&time).unwrap();
							let date = if now.date() != time.date() && (dated.is_none() || dated.unwrap() != time.date()) {
								let date = time.date();
								dated = Some(date);
								date.format_localized("%A, %e. %B", locale()).to_string().into()
							} else {
								Default::default()
							};

							let ui_lectures: Vec<_> = lectures.into_iter().map(Into::into).collect();
							ui::TimeBlock {
								date: date,
								time: time.format("%H:%M").to_string().into(),
								lectures: VecModel::from_slice(&ui_lectures),
								..Default::default()
							}
						})
						.collect();
					ui.set_time_blocks(VecModel::from_slice(&ui_blocks));
				});
				log::info!("next lecture update at {}", now + next_update);
				time::sleep(next_update.to_std().unwrap().into()).await;
			}
		}
	});

	let _news_updater: task::JoinHandle<Result<()>> = spawn({
		let ui = ui.clone();
		let client = client.clone();
		let url = opt.url.join("news.json").unwrap();
		async move {
			 loop {
				log::debug!("Loading headlines from {} ...", &url);
				ui.clone().upgrade_in_event_loop(move |ui| {
					ui.set_headlines_status("Loading...".into());
				});
				let response: Result<Vec<Headline>> = client.get(url.clone())
					.send()
					.and_then(http::Response::json)
					.await.context("failed to load headlines, reloading in 10s");

				match response {
					Ok(headlines) => {
						ui.clone().upgrade_in_event_loop(move |ui| {
							ui.set_headlines_status(Default::default());

							let ui_headlines: Vec<ui::Headline> = headlines.into_iter().map(Into::into).collect();
							ui.set_headlines(VecModel::from_slice(&ui_headlines));
						});
						time::sleep(time::Duration::from_secs(3600)).await;
					},
					Err(err) => {
						log::error!("{:#}", err);
						ui.clone().upgrade_in_event_loop(move |ui| {
							ui.set_headlines_status(error_showable(err));
						});
						time::sleep(time::Duration::from_secs(10)).await;
					},
				}
			}
		}
	});

	time_task.await?;
	Ok(())
}

fn error_showable(err: anyhow::Error) -> SharedString {
	let cause: String = err.chain().skip(1).take(1)
		.map(|err| err.to_string()).collect::<String>()
		.split(": ").collect::<Vec<_>>()
		.join("\n");

	format!("{}\n{}", err, cause).into()
}


#[derive(Debug, Deserialize)]
struct Headline {
	title: String,
	#[serde(deserialize_with="parse_unix_time")]
	time: chrono::NaiveDateTime,
}

impl Into<ui::Headline> for Headline {
	fn into(self) -> ui::Headline {
		ui::Headline {
			title: self.title.into(),
			date: self.time.format("%d.%m").to_string().into(),
			time: self.time.format("%H:%M").to_string().into(),
		}
	}
}

#[derive(Debug, Deserialize)]
struct Lecture {
	title: String,
	abbr: String,
	#[serde(deserialize_with="parse_unix_time")]
	time: chrono::NaiveDateTime,
	#[serde(default)]
	lecturers: Vec<String>,
	locations: Vec<String>,
	courses: Vec<String>,
}

fn parse_unix_time<'de, D>(deserializer: D) -> Result<chrono::NaiveDateTime, D::Error>
	where D: Deserializer<'de>
{
	let unix_ts = f64::deserialize(deserializer)? as i64;
	let time = chrono::Local.timestamp(unix_ts, 0).naive_local();
	Ok(time)
}

const COURSE_COLOR: &[(&str, Color)] = &[
	("IM", Color::from_argb_encoded(0xFF_BF3F2E)),
	("IN", Color::from_argb_encoded(0xFF_3D7EBF)),
	("IT", Color::from_argb_encoded(0xFF_308D38)),
	("IW", Color::from_argb_encoded(0xFF_B39336)),
	("MA", Color::from_argb_encoded(0xFF_6E8699)),
	("MIN", Color::from_argb_encoded(0xFF_1B3C8F)),
	("MMA", Color::from_argb_encoded(0xFF_37434D)),
	("KI", Color::from_argb_encoded(0xFF_792EB3)),
	("", Color::from_argb_encoded(0xFF_666666)),
];

const LOCATION_COLOR: &[(&str, Color)] = &[
	("K0", Color::from_argb_encoded(0xFF_4A78D3)),
	("K1", Color::from_argb_encoded(0xFF_365599)),
	("K2", Color::from_argb_encoded(0xFF_345494)),
	("virt", Color::from_argb_encoded(0xFF_B56713)),
	("", Color::from_argb_encoded(0xFF_4D4D4D)),
];

impl Into<ui::Lecture> for Lecture {
	fn into(self) -> ui::Lecture {
		let mut locations: Vec<_> = self.locations.into_iter()
			.map(|title| ui::Location {
				color: LOCATION_COLOR.iter()
					.find(|&(t, _color)| title.starts_with(t))
					.unwrap_or_else(|| LOCATION_COLOR.last().unwrap()).1,
				title: title.into(),
			})
			.collect();

		if locations.is_empty() {
			locations.push(ui::Location {
				title: "N/A".into(),
				color: LOCATION_COLOR.last().unwrap().1
			});
		}

		let courses: Vec<_> = self.courses.into_iter()
			.map(|title| ui::Course {
				color: COURSE_COLOR.iter()
					.find(|&(t, _color)| title.starts_with(t))
					.unwrap_or_else(|| COURSE_COLOR.last().unwrap()).1,
				title: title.into(),
			})
			.collect();

		ui::Lecture {
			title: self.title.into(),
			abbr: self.abbr.into(),
			lecturers: self.lecturers.join(",").into(),
			locations: VecModel::from_slice(&locations),
			courses: VecModel::from_slice(&courses),
		}
	}
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
