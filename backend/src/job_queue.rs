use std::sync::Arc;

use diesel::{
	BoolExpressionMethods, ExpressionMethods, OptionalExtension, QueryDsl, delete, insert_into,
	update,
};
use kstring::KString;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::{OffsetDateTime, PrimitiveDateTime};
use tracing::{info, warn};
use uuid::Uuid;

use crate::{
	Result,
	branch::BranchRef,
	db::{
		BoxedSqlConn,
		schema::job_queue::dsl,
		service::DatabaseService,
		utils::{XJsonVal, XUuidVal},
	},
};

#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
#[serde(tag = "t", content = "c", rename = "kebab-case")]
pub enum JobCommand {
	/// Synchronize metadata of a branch.
	SyncBranch(BranchRef),
}

impl JobCommand {
	pub fn serialize(&self) -> serde_json::Result<(KString, serde_json::Value)> {
		let mut value = serde_json::to_value(self)?;
		Ok((
			KString::from_ref(value["t"].as_str().unwrap()),
			value.as_object_mut().unwrap().remove("c").unwrap(),
		))
	}

	pub fn deserialize(kind: &str, value: serde_json::Value) -> serde_json::Result<Self> {
		let value = serde_json::json!({ "t": kind, "c": value });
		serde_json::from_value(value)
	}
}

pub type JobRef = Uuid;

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Job {
	pub id: JobRef,
	pub command: JobCommand,
}

#[derive(Debug)]
pub struct JobQueue {
	db: Arc<DatabaseService>,
}

impl JobQueue {
	pub fn new(db: Arc<DatabaseService>) -> Self {
		Self { db }
	}

	pub async fn enqueue(&self, conn: &mut BoxedSqlConn, job: JobCommand) -> Result<()> {
		self.enqueue_with_priority(conn, job, 100).await
	}

	pub async fn enqueue_with_priority(
		&self,
		conn: &mut BoxedSqlConn,
		job: JobCommand,
		priority: u16,
	) -> Result<()> {
		let id = Uuid::now_v7();
		let (kind, job_data) = job.serialize()?;

		let id = conn
			.get_result::<_, XUuidVal>(
				insert_into(dsl::job_queue)
					.values((
						dsl::id.eq(XUuidVal(id)),
						dsl::kind.eq(kind.as_str()),
						dsl::data.eq(XJsonVal(job_data)),
						dsl::priority.eq(priority as i16),
					))
					.returning(dsl::id),
			)
			.await?;
		let id = id.0;
		info!(%kind, %id, "enqueued job");

		// TODO: notify a job worker

		Ok(())
	}

	pub async fn fetch_and_start(&self) -> Result<Option<Job>> {
		let mut conn = self.db.get().await?;

		loop {
			let time = OffsetDateTime::now_utc();
			let time = PrimitiveDateTime::new(time.date(), time.time());

			// find a pending job
			// for jobs with the same priority, we order them with ID.
			// because ID are UUID v7, this is equivalent to ordering with
			// insertion time
			let result = conn
				.get_result::<_, (XUuidVal, String, XJsonVal)>(
					dsl::job_queue
						.limit(1)
						.filter(dsl::started_at.is_null())
						.order((dsl::priority.desc(), dsl::id.asc()))
						.select((dsl::id, dsl::kind, dsl::data)),
				)
				.await
				.optional()?;
			if let Some((id, kind, data)) = result {
				let cols = conn
					.execute(
						update(dsl::job_queue)
							.filter(dsl::id.eq(id).and(dsl::started_at.is_null()))
							.set(dsl::started_at.eq(time)),
					)
					.await?;
				#[cfg(test)]
				assert!(cols != 0);
				if cols == 0 {
					warn!(%id, "SQL lightweight job queue polling hit contented");
					continue;
				}
				info!(%id, "polled lightweight job");
				let cmd = JobCommand::deserialize(&kind, data.0)?;
				return Ok(Some(Job {
					id: id.0,
					command: cmd,
				}));
			} else {
				return Ok(None);
			}
		}
	}

	pub async fn finish_job(&self, conn: &mut BoxedSqlConn, id: JobRef) -> Result<()> {
		let cols = conn
			.execute(
				delete(dsl::job_queue)
					.filter(dsl::id.eq(XUuidVal(id)).and(dsl::started_at.is_not_null())),
			)
			.await?;
		if cols == 0 {
			warn!(%id, "job has been aborted or finished by another worker");
			return Err(JobQueueError::JobAborted(id).into());
		}
		Ok(())
	}

	/// Returns the approximate count of pending jobs.
	pub async fn count_pending(&self, max: usize) -> Result<usize> {
		let mut conn = self.db.get().await?;

		let count: i64 = conn
			.get_result(
				dsl::job_queue
					.count()
					.filter(dsl::started_at.is_not_null())
					.limit(max.try_into().unwrap()),
			)
			.await?;
		Ok(count.try_into().unwrap())
	}
}

#[derive(Debug, Error)]
pub enum JobQueueError {
	#[error("job {0} has been aborted")]
	JobAborted(JobRef),
}

#[cfg(test)]
mod test {
	use diesel::QueryDsl;

	use crate::{db::schema::job_queue::dsl, job_queue::JobCommand, test::test_env};

	#[tokio::test]
	async fn test_enqueue() {
		let env = test_env().await;
		let mut db = env.database.get().await.unwrap();
		env.job_queue
			.enqueue(&mut db, JobCommand::SyncBranch(1))
			.await
			.unwrap();
	}

	#[tokio::test]
	async fn test_enqueue_fetch() {
		let env = test_env().await;
		let mut db = env.database.get().await.unwrap();
		let jq = env.job_queue;
		jq.enqueue(&mut db, JobCommand::SyncBranch(1))
			.await
			.unwrap();
		jq.enqueue_with_priority(&mut db, JobCommand::SyncBranch(2), 120)
			.await
			.unwrap();
		jq.enqueue(&mut db, JobCommand::SyncBranch(3))
			.await
			.unwrap();
		drop(db);
		assert_eq!(
			jq.fetch_and_start().await.unwrap().unwrap().command,
			JobCommand::SyncBranch(2)
		);
		assert_eq!(
			jq.fetch_and_start().await.unwrap().unwrap().command,
			JobCommand::SyncBranch(1)
		);
		assert_eq!(
			jq.fetch_and_start().await.unwrap().unwrap().command,
			JobCommand::SyncBranch(3)
		);
		assert!(jq.fetch_and_start().await.unwrap().is_none());
	}

	#[tokio::test]
	async fn test_finish() {
		let env = test_env().await;
		let jq = env.job_queue;

		let mut db = env.database.get().await.unwrap();
		jq.enqueue(&mut db, JobCommand::SyncBranch(1))
			.await
			.unwrap();
		drop(db);

		let id = jq.fetch_and_start().await.unwrap().unwrap().id;

		let mut db = env.database.get().await.unwrap();
		jq.finish_job(&mut db, id).await.unwrap();
		assert_eq!(
			db.get_result::<_, i64>(dsl::job_queue.count())
				.await
				.unwrap(),
			0
		);
		drop(db);

		assert!(jq.fetch_and_start().await.unwrap().is_none());
	}
}
