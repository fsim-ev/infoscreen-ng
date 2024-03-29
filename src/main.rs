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

	let ui = ui::App::new()?;
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

	if let Err(err) = ui.run() {
		log::error!(err=%err, "failed to start UI task...");
	} else {
		log::debug!("UI task shutting down...");
	}
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

	let rt = runtime::Builder::new_current_thread()
		.enable_all()
		.thread_name("io-worker")
		.build()?;

	rt.block_on(io_run(ui, conf, run_token))?;
	rt.shutdown_timeout(time::Duration::from_secs(10));

	Ok(())
}

async fn io_run(ui: Weak<ui::App>, mut conf: Config, run_token: CancellationToken) -> Result<()>
{
	use tokio::*;

	let timetable_colors = TimeEntryColorSet {
		course: std::mem::take(&mut conf.course_colors.map).into_iter()
			.map(|(rgx_str, color_hex)|
				(
					Regex::new(&rgx_str).expect("failed to compile course regex"),
					Color::from_argb_encoded(color_hex),
				)
			)
			.collect(),
		course_default: Color::from_argb_encoded(conf.course_colors.default),

		location: std::mem::take(&mut conf.location_colors.map).into_iter()
			.map(|(rgx_str, color_hex)|
				(
					Regex::new(&rgx_str).expect("failed to compile location regex"),
					Color::from_argb_encoded(color_hex),
				)
			)
			.collect(),
		location_default: Color::from_argb_encoded(conf.location_colors.default),
	};

	let exam_end = Arc::new(sync::RwLock::new(Local::now()));

	let time_task = spawn({
		let ui = ui.clone();
		let exam_end = exam_end.clone();
		async move {
			let old_ts = Local::now() - Duration::days(1) - Duration::minutes(1);
			let mut old_date = old_ts.date_naive();
			let mut old_time = old_ts.time();
			loop {
				let sleep_til = time::Instant::now() + time::Duration::from_millis(100);
				let now = Local::now() + Duration::days(conf.day_offset as _);

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
					let date_fmt = if date.month() == 1 { "%A, %e. %B %Y" } else { "%A, %e. %B" };
					let date_str = date.format_localized(date_fmt, locale()).to_string();
					old_date = date;
					Some(date_str)
				} else {
					None
				};

				let exam_end = *exam_end.read().await;
				let exam_time_left = (exam_end - now).num_milliseconds();
				let o_time_left = (Local.with_ymd_and_hms(2023, 7, 28, 12, 0, 0).unwrap() - now).num_milliseconds();
				let o_time_left_str = if o_time_left > 0 { format!("{:0.8}", o_time_left as f32 / 1000.0 / 84600.0) } else { Default::default() };

				ui.upgrade_in_event_loop(move |ui| {
					ui.set_secs(now.second() as i32);
					ui.set_o_time_left_str(o_time_left_str.into());
					if exam_time_left >= 0 {
						ui.set_exam_time_left(exam_time_left as _);
						ui.set_exam_time_left_str(format!("{:0.8}", exam_time_left as f32 / 1000.0 / 84600.0).into());
					}
					if let Some(date) = new_date {
						ui.set_date(date.into());
					}
					if let Some(time) = new_time {
						ui.set_time(time.into());

						let day_time = now.hour() * 100 + now.minute();
						ui.global::<ui::State>()
							.set_day_time(day_time as _);

						ui.set_time_off(day_time >= conf.time_off.start || day_time < conf.time_off.end);
					}
				}).ok();
				time::sleep_until(sleep_til).await;
			};
		}
	});

	let (tx, mut new_entries) = sync::mpsc::channel::<Vec<TimeEntry>>(4);

	let table_updater: task::JoinHandle<Result<()>> = spawn({
		let ui = ui.clone();
		let exam_end = exam_end.clone();
		async move {
			let mut is_exam_phase = false;
			let mut entries = Vec::with_capacity(64);

			while let Some(new_entries) = new_entries.recv().await {
				entries.extend(new_entries);

				let now = Local::now() + Duration::days(conf.day_offset as _);
				log::debug!("current date: {}", now.to_rfc2822());

				entries.sort_by(|a, b| a.time_start.cmp(&b.time_start));

				let past_idx = entries.iter()
					.enumerate()
					.rev()
					.find_map(|(idx, entry)| (entry.time_end < now).then(|| idx));
				if let Some(idx) = past_idx {
					let entry = entries.get(idx).unwrap();
					log::debug!("draining at {idx} past {}", entry.time_end);
					entries.drain(..=idx);
				}

				ui.upgrade_in_event_loop(move |ui| {
					ui.set_loading(false);
					ui.set_timetable_status(Default::default());
				}).ok();

				if entries.is_empty() {
					log::info!("loaded 0 entries");
					continue;
				} else {
					let first_entry = entries.first().unwrap();
					is_exam_phase = first_entry.is_exam;

					let first_time = first_entry.time_start.to_rfc2822();
					let last_time = entries.last().unwrap().time_start.to_rfc2822();
					log::debug!("loaded {} entries (first: {}, last: {})", entries.len(), first_time, last_time);
				}

				let mut time_blocks: std::collections::BTreeMap<_, Vec<TimeEntry>> = Default::default();
				for entry in entries.iter()
				{
					let skip_after = if is_exam_phase {
						entry.time_end
					} else {
						entry.time_start + Duration::minutes(30)
					};

					if skip_after < now {
						continue;
					}
					time_blocks.entry(entry.time_start)
						.or_default()
						.push(entry.clone());
				}

				time_blocks.values_mut().for_each(|entries|
					entries.sort_by(|a, b| a.abbr.cmp(&b.abbr))
				);

				let timetable_colors = timetable_colors.clone();
				ui.upgrade_in_event_loop(move |ui| {
					let mut dated = None;
					let ui_blocks: Vec<ui::TimeBlock> = time_blocks.into_iter()
						.map(|(time, entries)| {
							let date = (now.date_naive() != time.date_naive() && (dated.is_none() || dated.unwrap() != time.date_naive()))
								.then(|| {
									let date = time.date_naive();
									dated = Some(date);
									date.format_localized("%A, %e. %B", locale()).to_string().into()
								})
								.unwrap_or_default();

							let ui_entries: Vec<_> = entries.into_iter()
								.map(|entry| entry.into_ui(&timetable_colors))
								.collect();
							ui::TimeBlock {
								date,
								time: time.format("%H:%M").to_string().into(),
								entries: VecModel::from_slice(&ui_entries),
								..Default::default()
							}
						})
						.collect();

					ui.set_time_blocks(VecModel::from_slice(&ui_blocks));
				}).ok();
			}
			Ok(())
		}
	});


	let exam_updater: task::JoinHandle<Result<()>> = spawn({
		let ui = ui.clone();
		let tx = tx.clone();
		async move {
			let mut sleep_dur = time::Duration::ZERO;
			let mut is_exam_phase = false;

			loop {
				if !sleep_dur.is_zero() {
					let now = Local::now() + Duration::days(conf.day_offset as _);
					let dur = Duration::from_std(sleep_dur).unwrap();
					log::info!("next time table update at {} (in {} min)", (now + dur).to_rfc2822(), dur.num_minutes());
				}
				time::sleep(sleep_dur).await;
				sleep_dur = time::Duration::from_secs(30);

				let now = Local::now() + Duration::days(conf.day_offset as _);
				log::debug!("current date: {}", now.to_rfc2822());

				log::debug!("loading exams...");
				let mut entries = match fetch_exam_times().await {
					Ok(v) => v,
					Err(err) => {
						log::error!("{:#}", err);
						if is_exam_phase {
							ui.upgrade_in_event_loop(move |ui| {
								ui.set_timetable_status(error_showable(err));
							}).ok();
							continue;
						}
						vec![]
					},
				};
				log::debug!("loaded {} exams", entries.len());
				if entries.is_empty() {
					continue;
				}

				entries.sort_by(|a,b| a.time_end.cmp(&b.time_end));

				if let Some((start, end)) = entries.first().zip(entries.last()) {
					is_exam_phase = start.time_start <= now && now < end.time_end;
					let exam_end_time = end.time_start.clone();
					let mut ee = exam_end.write().await;
					*ee = exam_end_time;
				}
				if let Some(dur) = entries.last()
					.and_then(|entry| (entry.time_end - now).to_std().ok()) {
					sleep_dur = dur;
				}
				tx.send(entries).await.ok();
			}
		}
	});

	if let Some(untis_conf) = conf.untis.clone() {
		let task: task::JoinHandle<Result<()>> = spawn({
			let ui = ui.clone();
			let tx = tx.clone();
			let untis_conf = untis_conf.clone();
			async move {

				let mut sleep_dur = time::Duration::ZERO;
				let mut lecture_base = None;

				loop {
					if !sleep_dur.is_zero() {
						let now = Local::now() + Duration::days(conf.day_offset as _);
						let dur = Duration::from_std(sleep_dur).unwrap();
						log::info!("next lecture table update at {} (in {} min)", (now + dur).to_rfc2822(), dur.num_minutes());
					}
					time::sleep(sleep_dur).await;
					sleep_dur = time::Duration::from_secs(30);

					let now = Local::now() + Duration::days(conf.day_offset as _);

					log::debug!("loading lectures...");

					let untis_conf = untis_conf.clone();
					let session = untis::Session::create(
							&untis_conf.school,
							untis_conf.auth.username.clone().into(),
							untis_conf.auth.password.clone().into(),
						)
						.await
						.context("failed to create Untis session")?;

					if lecture_base.is_none() {
						log::debug!("loading lecture base data...");
						lecture_base = match fetch_lecture_base_data(&session, &untis_conf.faculty).await {
							Ok(v) => Some(v),
							Err(err) => {
								let err = anyhow::format_err!("failed to load lecture base data: {err}");
								log::error!("{:#}", err);
								ui.upgrade_in_event_loop(move |ui| {
									ui.set_timetable_status(error_showable(err));
								}).ok();
								continue;
							},
						};
					}
					log::debug!("loading lecture timetable...");
					let entries = match fetch_lecture_times(&session, lecture_base.as_ref().unwrap(), &untis_conf, conf.day_offset).await {
						Ok(v) => v,
						Err(err) => {
							let err = anyhow::format_err!("failed to load lecture timetable: {err}");
							log::error!("{:#}", err);
							ui.upgrade_in_event_loop(move |ui| {
								ui.set_timetable_status(error_showable(err));
							}).ok();
							lecture_base = None;
							continue;
						},
					};
					log::debug!("loaded {} lectures", entries.len());
					if let Some(dur) = entries.iter()
							.max_by(|&a, &b| a.time_start.cmp(&b.time_start))
							.and_then(|entry| (entry.time_end - now).to_std().ok()) {
						sleep_dur = dur;
					}
					tx.send(entries).await.ok();
				}
			}
		});
		if let Err(err) = task.await {
			let err = if err.is_panic() {
				panic_description(err.into_panic())
			} else {
				err.to_string()
			};
			let err = anyhow::format_err!("failed to update lectures: {}", err);
			ui.upgrade_in_event_loop(move |ui| {
				ui.set_timetable_status(error_showable(err));
			}).ok();
		}
	};

	if let Some(newsfeed_url) = conf.newsfeed_url {
		let task: task::JoinHandle<Result<()>> = spawn({
			let ui = ui.clone();
			let client = http::Client::new();
			let request = client.get(newsfeed_url).build()?;
			async move {
				loop {
					log::debug!("Loading headlines from {} ...", request.url().as_str());
					ui.upgrade_in_event_loop(move |ui| {
						ui.set_headlines_status("Loading...".into());
					}).ok();
					let request = request.try_clone()
						.context("failed to clone request")?;
					let response = client.execute(request)
						.and_then(http::Response::bytes)
						.await
						.context("failed to load headlines, reloading in 10s")
						.and_then(|data|
							feed_rs::parser::parse(data.reader())
								.context("failed to parse feed")
						);

					match response {
						Ok(feed) => {
							ui.upgrade_in_event_loop(move |ui| {
								ui.set_headlines_status(Default::default());

								let ui_headlines: Vec<_> = feed.entries.into_iter()
									.filter_map(|e| e.title.zip(e.published))
									.map(|(title, published)| Headline {
										title: title.content
											.trim_end_matches(':').trim()
											.to_owned().into(),
										time: published.into()
									})
									.map(Into::into)
									.collect();
								ui.set_headlines(VecModel::from_slice(&ui_headlines));
							}).ok();
							time::sleep(time::Duration::from_secs(3600)).await;
						},
						Err(err) => {
							log::error!("{:#}", err);
							ui.upgrade_in_event_loop(move |ui| {
								ui.set_headlines_status(error_showable(err));
							}).ok();
							time::sleep(time::Duration::from_secs(10)).await;
						},
					}
				}
			}
		});
		if let Err(err) = task.await {
			let err = if err.is_panic() {
				panic_description(err.into_panic())
			} else {
				err.to_string()
			};
			let err = anyhow::format_err!("failed to update headlines: {}", err);
			ui.upgrade_in_event_loop(move |ui| {
				ui.set_headlines_status(error_showable(err));
			}).ok();
		}
	}


	tokio::select!{
		res = table_updater => if let Err(err) = res {
			let err = if err.is_panic() {
				panic_description(err.into_panic())
			} else {
				err.to_string()
			};
			let err = anyhow::format_err!("failed to update table: {}", err);
			log::error!("{}", err);
			ui.upgrade_in_event_loop(move |ui| {
				ui.set_timetable_status(error_showable(err));
			}).ok();
		},
		res = exam_updater => if let Err(err) = res {
			let err = if err.is_panic() {
				panic_description(err.into_panic())
			} else {
				err.to_string()
			};
			let err = anyhow::format_err!("failed to update exams: {}", err);
			log::error!("{}", err);
			ui.upgrade_in_event_loop(move |ui| {
				ui.set_timetable_status(error_showable(err));
			}).ok();
		},
		_ = tokio::signal::ctrl_c() => {
			log::debug!("SIGINT detected!");
		},
		_ = run_token.cancelled() => {},
	};

	log::debug!("IO task shutting down...");
	Ok(())
}

fn error_showable(err: anyhow::Error) -> SharedString {
	let cause: String = err.chain().skip(1).take(1)
		.map(|err| err.to_string()).collect::<String>()
		.split(": ").collect::<Vec<_>>()
		.join("\n");

	let mut err_str = err.to_string();
	if !cause.is_empty() {
		err_str.extend(['\n'].into_iter());
		err_str.extend(cause.chars());
	}
	err_str.into()
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


#[derive(Debug)]
struct Headline {
	title: String,
	time: DateTime<Local>,
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

// Timetable
//###########
#[derive(Debug, Clone)]
pub struct TimeEntry {
	title: String,
	abbr: String,
	time_start: DateTime<Local>,
	time_end: DateTime<Local>,
	locations: BTreeSet<String>,
	courses: BTreeSet<String>,
	is_exam: bool,
}

#[derive(Debug, Clone)]
pub struct TimeEntryColorSet {
	course: Vec<(Regex, Color)>,
	course_default: Color,
	location: Vec<(Regex, Color)>,
	location_default: Color,
}

impl TimeEntry {
	fn into_ui(self, colors: &TimeEntryColorSet) -> ui::Entry {
		let mut locations: Vec<_> = self.locations.into_iter()
			.map(|title| ui::Location {
				color: colors.location.iter()
					.find(|&(rgx, _color)| rgx.is_match(&title))
					.map(|(_, color)| color.clone())
					.unwrap_or_else(|| colors.location_default),
				title: title.into(),
			})
			.collect();

		if locations.is_empty() {
			locations.push(ui::Location {
				title: "N/A".into(),
				color: colors.location_default
			});
		}

		let courses: Vec<_> = self.courses.into_iter()
			.map(|title| ui::Course {
				color: colors.course.iter()
					.find(|(rgx, _color)| rgx.is_match(&title))
					.map(|(_, color)| color.clone())
					.unwrap_or_else(|| colors.course_default),
				title: title.into(),
			})
			.collect();

		ui::Entry {
			title: self.title.into(),
			abbr: self.abbr.into(),
			is_exam: self.is_exam,
			lecturers: "".into(), // TODO: self.lecturers.join(",").into(),
			locations: VecModel::from_slice(&locations),
			courses: VecModel::from_slice(&courses),
		}
	}
}

struct UntisData {
	classes: HashMap<u32, untis::Class>,
	subjects: HashMap<u32, untis::Subject>,
	rooms: HashMap<u32, untis::Room>,
}

async fn fetch_lecture_base_data(session: &untis::Session, faculty: &str) -> Result<UntisData> {
	let dep_id = session.departments()
		.await?
		.into_iter()
		.find_map(|dep| (dep.name == faculty).then(|| dep.id))
		.context("failed to find department")?;

	let classes: HashMap<u32, untis::Class> = session.classes()
		.await?
		.into_iter()
		.filter(|e| e.active)
		.filter(|c| c.did.map(|did| did == dep_id).unwrap_or(false))
		.map(|c| (c.id, c))
		.collect();

	let subjects: HashMap<u32, untis::Subject> = session.subjects()
		.await?
		.into_iter()
		.filter(|e| e.active)
		.map(|e| (e.id, e))
		.collect();

	let rooms: HashMap<u32, untis::Room> = session.rooms()
		.await?
		.into_iter()
		.filter(|e| e.active)
		.map(|e| (e.id, e))
		.collect();

	Ok(UntisData {classes, subjects, rooms})
}

async fn fetch_lecture_times(session: &untis::Session, data: &UntisData, conf: &UntisConfig, day_offset: i16) -> Result<Vec<TimeEntry>>
{
	let today = Local::now().date_naive() + Duration::days(day_offset as _);
	let tomorrow = today + Duration::days(conf.day_range as _);

	// let terms = session.school_years().await?; // TODO: fix fetch error during semester overlap
	let faculty_prefix = conf.faculty.clone() + "_";

	let mut lectures: HashMap<u32, TimeEntry> = Default::default();
	for class in data.classes.values() {
		log::debug!("loading timetable for {}...", class.name);
		let class_lectures = session.timetable(untis::TimetableType::Class, class.id, today, tomorrow).await?;
		for lecture in class_lectures {
			let subject = match lecture.subject_ids.first() {
				Some(id) => data.subjects.get(&id).unwrap(),
				None => continue,
			};
			let rooms: Vec<_> = lecture.room_ids.into_iter()
				.filter_map(|id| data.rooms.get(&id).map(|r| r.name.clone().into()))
				.collect();

			let time_start = Local.from_local_datetime(&lecture.date.and_time(lecture.start_time))
				.latest()
				.context("failed to convert lecture start time")?;

			let time_end = Local.from_local_datetime(&lecture.date.and_time(lecture.end_time))
				.latest()
				.context("failed to convert lecture end time")?;

			let l = lectures.entry(lecture.id)
				.or_insert(TimeEntry {
					title: subject.long_name.clone().into(),
					abbr: subject.name.trim_start_matches(&faculty_prefix).clone().into(),
					is_exam: false,
					time_start, time_end,
					locations: Default::default(),
					courses: Default::default(),
				});

			l.courses.insert(class.name.clone().into());
			l.locations.extend(rooms);
		}
	}
	Ok(lectures.into_values().collect())
}

async fn fetch_exam_times() -> Result<Vec<TimeEntry>> {
	let session = othr_ptp::Session::create(othr_ptp::Faculty::IM).await
		.context("failed to create exam session")?;

	session.fetch().await
		.context("failed to fetch exam times")
}

