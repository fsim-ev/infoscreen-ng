#![allow(dead_code)]

use std::collections::{BTreeSet, HashMap};

use chrono::{Datelike, Local, TimeZone};
use anyhow::{Result, Context};
use reqwest as http;
use slint::SharedString;
use tokio::time;
use scraper::{Html, Selector};
use tracing::log;

use crate::TimeEntry;

#[derive(Copy, Clone, Debug)]
pub enum Faculty {
	A = 1,
	ANK = 2,
	B = 3,
	BW = 4,
	EI = 5,
	IM = 6,
	M = 7,
}

pub struct Session {
	client: http::Client,
}

impl Session {
	const URL_BASE: &'static str = "https://ptp.othr.de/ptp";

	pub async fn create(faculty: Faculty) -> Result<Self> {
		let client = http::ClientBuilder::new()
			.connect_timeout(time::Duration::from_secs(10))
			.timeout(time::Duration::from_secs(20))
			.cookie_store(true)
			.build()
			.context("failed to init http client")?;

		let url = [Self::URL_BASE, "/login/guest.xhtml"].concat();
		let fid = (faculty as u32).to_string();

		let forms = HashMap::from([
			("javax.faces.partial.ajax", "true"),
			("javax.faces.source", "form:loginGuestProject"),
			("javax.faces.partial.execute", "form:loginGuestProject"),
			("javax.faces.partial.render", "form"),
			("javax.faces.behavior.event", "valueChange"),
			("javax.faces.partial.event", "change"),
			("javax.faces.ViewState", "stateless"),
			("form", "form"),
			("form:loginGuestProject_input", &fid),
			("form:loginUsername", ""),
			("form:loginPassword", ""),
		]);
		let xml = client.post(&url)
			.form(&forms)
			.send()
			.await
			.and_then(|res| res.error_for_status())
			.context("failed to send parameters")?
			.text()
			.await
			.context("failed to parse parameter response")?;

		let (_, xml) = xml.split_once("<button id=\"")
			.context("form id start not found")?;

		let (form_id, _) = xml.split_once('"')
			.context("form id end not found")?;

		log::debug!("form id: {form_id}");

		let forms = HashMap::from([
			("jakarta.faces.ViewState", "stateless"),
			("form", "form"),
			("form:loginGuestProject_input", &fid),
			(form_id, ""),
		]);
		client.post(&url)
			.form(&forms)
			.send()
			.await
			.and_then(|res| res.error_for_status())
			.context("failed to login")?;

		Ok(Session { client })
	}

	pub async fn fetch(&self) -> Result<Vec<TimeEntry>> {

		let sel_exam = Selector::parse("table.autoTable > tbody > tr.ui-widget-content > td.tableColumn").unwrap();
		let sel_time = Selector::parse("div.inner.head:first-child > span:first-child").unwrap();
		let sel_room = Selector::parse("div.inner.head:first-child > span:last-child").unwrap();

		let sel_course = Selector::parse("div.inner.head > span > span:first-child").unwrap();
		let sel_lecture = Selector::parse("div.inner > span > span > a").unwrap();

		let html = self.client.get([Self::URL_BASE, "/view/general/rooms.xhtml"].concat())
			.send()
			.await
			.and_then(|res| res.error_for_status())
			.context("failed to request data")?
			.text()
			.await
			.context("failed to fetch data")?;

		let current_year = Local::today().year(); // meme eternalized

		let mut exams: HashMap<_, TimeEntry> = Default::default();

		let doc = Html::parse_document(&html);
		for cell_cel in doc.select(&sel_exam) {
			let time = match cell_cel.select(&sel_time).next() {
				None => continue,
				Some(el) => {
					// Fr.&nbsp;08.07., 14:15
					let text = el.inner_html();
					let mut date_str = text.split_once(';')
						.map(|(_, date)| date)
						.unwrap_or(&text)
						.trim()
						.to_owned();

					date_str.push_str(&format!(" {current_year}")); // ensure full date
					Local.datetime_from_str(&date_str, "%d.%m., %H:%M %Y").unwrap()
				},
			};

			let room = match cell_cel.select(&sel_room).next() {
				None => continue,
				Some(el) => el.inner_html().trim().replace("-Foyer", "000"),
			};

			cell_cel.select(&sel_course)
				.zip(cell_cel.select(&sel_lecture))
				.map(|(course_el, exam_el)| {
					let mut locations: BTreeSet<SharedString> = Default::default();
					locations.insert(room.clone().into());

					let mut courses: BTreeSet<SharedString> = Default::default();
					courses.insert(course_el.inner_html().trim().into());

					let title = exam_el.value().attr("title")
						.and_then(|attr| attr.splitn(4, char::is_whitespace).last())
						.unwrap_or_default();

					TimeEntry {
						title: title.into(),
						abbr: exam_el.inner_html().trim().into(),
						time: time.clone(), locations, courses,
					}
				})
				//.inspect(|lect| log::debug!("{:?}", lect))
				.for_each(|lect| {
					let key = (lect.time, lect.abbr.clone(), lect.courses.iter().next().unwrap().clone());
					exams.entry(key)
						.and_modify(|l| l.locations.extend(lect.clone().locations.into_iter()))
						.or_insert(lect);
				});

		}
		Ok(exams.into_values().collect())
	}
}


