#![allow(dead_code)]

use std::borrow::Cow;
use chrono::{Datelike, NaiveDate, NaiveTime};
use anyhow::{Result, Context};
use reqwest as http;
use tokio::time;
use serde::{Deserialize, Serialize, de::{Deserializer, DeserializeOwned}};
use json_rpc_types as jrpc;
use tracing::log;

pub struct Session {
	client: http::Client,
	school: String,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
struct Empty {}

impl Session {
	const URL: &'static str = "https://kephiso.webuntis.com/WebUntis/jsonrpc.do";

	pub async fn create<'a>(school: &str, user: Cow<'a, str>, pass: Cow<'a, str>) -> Result<Self> {
		let client = http::ClientBuilder::new()
			.connect_timeout(time::Duration::from_secs(10))
			.timeout(time::Duration::from_secs(20))
			.cookie_store(true)
			.build()
			.context("failed to init http client")?;

		let session = Session { client, school: school.to_owned() };

		#[derive(Debug, Clone, Serialize)]
		struct Params <'a> {
			user: Cow<'a, str>,
			password: Cow<'a, str>,
		}
		session.req::<_,Empty>("authenticate", Params { user, password: pass })
			.await?;

		Ok(session)
	}

	pub async fn departments(&self) -> Result<Vec<Department>> {
		self.req("getDepartments", ()).await
	}

	pub async fn rooms(&self) -> Result<Vec<Room>> {
		self.req("getRooms", ()).await
	}


	pub async fn school_years(&self) -> Result<Vec<SchoolYear>> {
		self.req("getSchoolyears", ()).await
	}

	pub async fn subjects(&self) -> Result<Vec<Subject>> {
		self.req("getSubjects", ()).await
	}

	pub async fn classes(&self) -> Result<Vec<Class>> {
		self.req("getKlassen", ()).await
	}

	pub async fn timetable(&self, filter_type: TimetableType, filter_id: u32, start: NaiveDate, end: NaiveDate) -> Result<Vec<Lecture>> {
		#[derive(Debug, Clone, Serialize)]
		struct Params {
			r#type: u32,
			id: u32,
			#[serde(rename = "startDate")]
			start_date: u32,
			#[serde(rename = "endDate")]
			end_date: u32,
		}
		let params = Params {
			r#type: filter_type as u32,
			id: filter_id,
			start_date: start.year() as u32 * 10000 + start.month() * 100 + start.day(),
			end_date: end.year() as u32 * 10000 + end.month() * 100 + end.day(),
		};
		self.req("getTimetable", params).await
	}

	async fn req<Req, Res>(&self, method: &'static str, req: Req) -> Result<Res>
		where Req: Serialize + std::fmt::Debug,
			Res: DeserializeOwned,
	{
		let req = jrpc::Request {
			jsonrpc: jrpc::Version::V2,
			id: Some(jrpc::Id::Num(1)),
			method,
			params: req.into(),
		};

		log::debug!("request: {} with {:?}", &req.method, &req.params);

		if log::log_enabled!(log::Level::Trace) {
			let text = self.client.get(Self::URL)
				.query(&[("school", &self.school)])
				.json(&req)
				.send()
				.await
				.and_then(|res| res.error_for_status())
				.context("failed to fetch data")?
				.text().await
				.context("failed to load body")?;

			log::debug!("response: {}", text);
		}

		self.client.get(Self::URL)
			.query(&[("school", &self.school)])
			.json(&req)
			.send()
			.await
			.and_then(|res| res.error_for_status())
			.context("failed to fetch data")?
			.json::<jrpc::Response<Res, String, String>>()
			.await
			.context("failed to parse data")?
			.payload.map_err(|err|
				anyhow::format_err!("request failed: {} - {}", err.code.code(), err.message)
			)
	}
}

#[derive(Debug, Clone, Copy, Serialize)]
pub enum TimetableType {
    Class = 1,
    Teacher = 2,
    Subject = 3,
    Room = 4,
    Student = 5,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Department {
	pub id: u32,
	pub name: String,
}
#[derive(Clone, Debug, Deserialize)]
pub struct SchoolYear {
	pub id: u32,
	pub name: String,
	#[serde(rename = "startDate", deserialize_with="parse_untis_date")]
	pub start_date: NaiveDate,
	#[serde(rename = "endDate", deserialize_with="parse_untis_date")]
	pub end_date: NaiveDate,
}
#[derive(Clone, Debug, Deserialize)]
pub struct Subject {
	pub id: u32,
	pub name: String,
	#[serde(rename = "longName")]
	pub long_name: String,
	pub active: bool,
}
#[derive(Clone, Debug, Deserialize)]
pub struct Class {
	pub id: u32,
	pub name: String,
	#[serde(rename = "longName")]
	pub long_name: String,
	pub active: bool,
	pub did: Option<u32>,
}
#[derive(Clone, Debug, Deserialize)]
pub struct Room {
	pub id: u32,
	pub name: String,
	#[serde(rename = "longName")]
	pub long_name: String,
	pub building: String,
	pub active: bool,
	pub did: Option<u32>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Lecture {
	pub id: u32,
	#[serde(deserialize_with="parse_untis_date")]
	pub date: NaiveDate,
	#[serde(rename = "startTime", deserialize_with="parse_untis_time")]
	pub start_time: NaiveTime,
	#[serde(rename = "endTime", deserialize_with="parse_untis_time")]
	pub end_time: NaiveTime,

	#[serde(rename = "kl", deserialize_with="parse_subids")]
	pub class_ids: Vec<u32>,
	#[serde(rename = "te", deserialize_with="parse_subids")]
	pub lecturer_ids: Vec<u32>,
	#[serde(rename = "su", deserialize_with="parse_subids")]
	pub subject_ids: Vec<u32>,
	#[serde(rename = "ro", deserialize_with="parse_subids")]
	pub room_ids: Vec<u32>,
}

fn parse_untis_time<'de, D>(deserializer: D) -> Result<NaiveTime, D::Error>
	where D: Deserializer<'de>
{
	let t = u32::deserialize(deserializer)?;
	Ok(NaiveTime::from_hms(t / 100, t % 100, 0))
}

fn parse_untis_date<'de, D>(deserializer: D) -> Result<NaiveDate, D::Error>
	where D: Deserializer<'de>
{
	let d = u32::deserialize(deserializer)?;
	Ok(NaiveDate::from_ymd(d as i32 / 10000 , d / 100 % 100, d % 100))
}

fn parse_subids<'de, D>(deserializer: D) -> Result<Vec<u32>, D::Error>
	where D: Deserializer<'de>
{
	#[derive(Debug, Deserialize)]
	struct Id {
		id: u32
	}
	type IdList = Vec<Id>;

	let list = IdList::deserialize(deserializer)?;
	Ok(list.into_iter().map(|s| s.id).collect())
}

