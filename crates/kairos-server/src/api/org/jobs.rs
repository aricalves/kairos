//! `GET /api/admin/jobs` (KAIROS-T-0576): the job queue for an
//! organization admin. The jobs by kind and state, and the last failures
//! with their error.
//!
//! The scope is the jobs of the organization of the caller and the jobs of
//! the deployment (no tenant: the periodic passes). The jobs of another
//! organization stay out, so an admin of one organization does not read
//! the errors of another. The route sits in the router of the
//! organization, behind the tenant layer, unlike `/api/admin/tenants`.
//!
//! The Prometheus gauges `kairos_jobs{kind,state}` count all the jobs of
//! the deployment ([`crate::metrics`]).

use axum::extract::{Extension, State};
use axum::routing::get;
use axum::{Json, Router};
use kairos_client::types_org::{
    JOB_ERROR_LIMIT, JOB_FAILURE_LIMIT, JobFailure, JobKindCounts, JobQueueStatus,
};
use kairos_db::jobs::{Job, KindStateCount};

use crate::api::meta::require_org_admin;
use crate::app::AppState;
use crate::error::ApiError;
use crate::middleware::tenant::TenantContext;

pub fn router() -> Router<AppState> {
    Router::new().route("/api/admin/jobs", get(get_jobs))
}

/// The job queue of the organization and of the deployment.
#[utoipa::path(
    get,
    path = "/api/admin/jobs",
    tag = "organization",
    responses(
        (status = 200, description = "The jobs by kind and state, and the last failures", body = JobQueueStatus),
        (status = 403, description = "The caller is not an organization admin", body = kairos_client::types::ErrorEnvelope),
    ),
)]
pub(crate) async fn get_jobs(
    State(state): State<AppState>,
    Extension(tenant): Extension<TenantContext>,
) -> Result<Json<JobQueueStatus>, ApiError> {
    require_org_admin(&tenant)?;
    let slug = tenant.slug.clone();
    let (counts, failures) = state
        .blocking
        .run_public(move |conn| {
            let counts = kairos_db::jobs::counts(conn, Some(&slug)).map_err(ApiError::internal)?;
            let failures =
                kairos_db::jobs::last_failures(conn, Some(&slug), JOB_FAILURE_LIMIT as i64)
                    .map_err(ApiError::internal)?;
            Ok((counts, failures))
        })
        .await?;
    Ok(Json(JobQueueStatus {
        kinds: by_kind(&counts),
        failures: failures.into_iter().map(failure).collect(),
    }))
}

/// One row for each kind, with the count of each state. The rows of
/// [`kairos_db::jobs::counts`] come by kind.
fn by_kind(counts: &[KindStateCount]) -> Vec<JobKindCounts> {
    let mut kinds: Vec<JobKindCounts> = Vec::new();
    for row in counts {
        if kinds.last().is_none_or(|last| last.kind != row.kind) {
            kinds.push(JobKindCounts {
                kind: row.kind.clone(),
                queued: 0,
                leased: 0,
                done: 0,
                failed: 0,
            });
        }
        let Some(last) = kinds.last_mut() else {
            continue;
        };
        match row.state.as_str() {
            "queued" => last.queued = row.count,
            "leased" => last.leased = row.count,
            "done" => last.done = row.count,
            "failed" => last.failed = row.count,
            _ => {}
        }
    }
    kinds
}

/// The wire form of a failed job, with its error cut to
/// [`JOB_ERROR_LIMIT`] characters.
fn failure(job: Job) -> JobFailure {
    JobFailure {
        id: job.id.to_string(),
        kind: job.kind,
        tenant: job.tenant,
        tries: job.tries,
        max_tries: job.max_tries,
        error: job.last_error.map(|error| cut(&error, JOB_ERROR_LIMIT)),
        finished_at: job.finished_at.map(|at| at.to_rfc3339()),
    }
}

/// `text`, cut to `limit` characters with `…` at the end when it is
/// longer.
fn cut(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count(kind: &str, state: &str, count: i64) -> KindStateCount {
        KindStateCount {
            kind: kind.to_string(),
            state: state.to_string(),
            count,
        }
    }

    #[test]
    fn the_counts_make_one_row_for_each_kind() {
        let rows = by_kind(&[
            count("periodic", "done", 3),
            count("periodic", "queued", 1),
            count("webhook_delivery", "failed", 2),
        ]);
        assert_eq!(
            rows,
            vec![
                JobKindCounts {
                    kind: "periodic".into(),
                    queued: 1,
                    leased: 0,
                    done: 3,
                    failed: 0,
                },
                JobKindCounts {
                    kind: "webhook_delivery".into(),
                    queued: 0,
                    leased: 0,
                    done: 0,
                    failed: 2,
                },
            ]
        );
    }

    #[test]
    fn a_long_error_is_cut_at_the_limit() {
        assert_eq!(cut("short", 10), "short");
        assert_eq!(cut("ééééé", 5), "ééééé");
        assert_eq!(cut("ééééééé", 5), "ééééé…");
    }
}
