//! The counts of the login throttle (KAIROS-T-0377, COLLIERY-I-0613).
//!
//! One table in the public schema (`public.auth_failures`), shared by every
//! server, so a lockout holds on each replica. A row is a subject: the
//! `kind` (`identity` or `source`) and the hash of its value. The rules are
//! those of KAIROS-T-0202:
//!
//! - failures count in a window; a failure after the window starts a new
//!   window, and forgets an old lockout;
//! - the failure that reaches `max_failures` locks the subject out for
//!   `lockout`, and the count starts again;
//! - a success removes the count of a subject that is not locked out; a
//!   live lockout stays.
//!
//! A check is one read of the primary key ([`status`]); a failure is one
//! upsert ([`record_failure`]); a success is one delete
//! ([`record_success`]), and only when the check found a row. Time is a
//! parameter.

use chrono::{DateTime, Utc};
use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::sql_query;
use diesel::sql_types::{Array, Bool, Double, Integer, Nullable, Text, Timestamptz};

/// A subject of the throttle: its kind (`identity` or `source`) and the
/// hash of its value.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SubjectKey {
    pub kind: String,
    pub subject: String,
}

/// The rules of the throttle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rules {
    pub max_failures: i32,
    pub window_secs: f64,
    pub lockout_secs: f64,
}

/// What the table holds for some subjects.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Status {
    /// The end of the longest live lockout of the subjects.
    pub locked_until: Option<DateTime<Utc>>,
    /// The subject of that lockout (KAIROS-T-0539): the caller says which
    /// limit refused the attempt.
    pub locked: Option<SubjectKey>,
    /// True when one of the subjects has a row.
    pub known: bool,
}

fn split(subjects: &[SubjectKey]) -> (Vec<String>, Vec<String>) {
    subjects
        .iter()
        .map(|s| (s.kind.clone(), s.subject.clone()))
        .unzip()
}

/// The lockout of `subjects` at `now`, and whether they have a row.
pub fn status(
    conn: &mut PgConnection,
    subjects: &[SubjectKey],
    now: DateTime<Utc>,
) -> QueryResult<Status> {
    #[derive(QueryableByName)]
    struct Row {
        #[diesel(sql_type = Nullable<Timestamptz>)]
        locked_until: Option<DateTime<Utc>>,
        #[diesel(sql_type = Nullable<Text>)]
        locked_kind: Option<String>,
        #[diesel(sql_type = Nullable<Text>)]
        locked_subject: Option<String>,
        #[diesel(sql_type = Bool)]
        known: bool,
    }
    if subjects.is_empty() {
        return Ok(Status::default());
    }
    let (kinds, keys) = split(subjects);
    // The kind and the subject of the longest live lockout. The two
    // aggregates have the same order, so they name the same row.
    let row: Row = sql_query(
        "SELECT max(f.locked_until) FILTER (WHERE f.locked_until > $3) AS locked_until, \
                (array_agg(f.kind ORDER BY f.locked_until DESC, f.kind, f.subject) \
                    FILTER (WHERE f.locked_until > $3))[1] AS locked_kind, \
                (array_agg(f.subject ORDER BY f.locked_until DESC, f.kind, f.subject) \
                    FILTER (WHERE f.locked_until > $3))[1] AS locked_subject, \
                count(*) > 0 AS known \
         FROM public.auth_failures f \
         JOIN unnest($1::text[], $2::text[]) AS t(kind, subject) \
           ON f.kind = t.kind AND f.subject = t.subject",
    )
    .bind::<Array<Text>, _>(kinds)
    .bind::<Array<Text>, _>(keys)
    .bind::<Timestamptz, _>(now)
    .get_result(conn)?;
    Ok(Status {
        locked_until: row.locked_until,
        locked: row
            .locked_kind
            .zip(row.locked_subject)
            .map(|(kind, subject)| SubjectKey { kind, subject }),
        known: row.known,
    })
}

/// Count a failure of `subjects` at `now`, by `rules`. True when the
/// failure locked one of them out.
pub fn record_failure(
    conn: &mut PgConnection,
    subjects: &[SubjectKey],
    now: DateTime<Utc>,
    rules: Rules,
) -> QueryResult<bool> {
    #[derive(QueryableByName)]
    struct Row {
        #[diesel(sql_type = Bool)]
        tripped: bool,
    }
    if subjects.is_empty() {
        return Ok(false);
    }
    let (kinds, keys) = split(subjects);
    // $3 now, $4 max_failures, $5 window, $6 lockout. In the update, `old`
    // is the window of the row that ran out, `n` the count with this
    // failure, and `trip` that `n` reached the limit; each is written out in
    // full where it is used.
    let rows: Vec<Row> = sql_query(
        "INSERT INTO public.auth_failures AS f \
             (kind, subject, failures, window_started, locked_until, expires_at) \
         SELECT t.kind, t.subject, \
                CASE WHEN $4 <= 1 THEN 0 ELSE 1 END, $3, \
                CASE WHEN $4 <= 1 THEN $3 + make_interval(secs => $6) END, \
                GREATEST($3 + make_interval(secs => $5), \
                         CASE WHEN $4 <= 1 THEN $3 + make_interval(secs => $6) END) \
         FROM unnest($1::text[], $2::text[]) AS t(kind, subject) \
         ON CONFLICT (kind, subject) DO UPDATE SET \
             failures = CASE \
                 WHEN (CASE WHEN f.window_started + make_interval(secs => $5) <= $3 \
                            THEN 0 ELSE f.failures END) + 1 >= $4 THEN 0 \
                 ELSE (CASE WHEN f.window_started + make_interval(secs => $5) <= $3 \
                            THEN 0 ELSE f.failures END) + 1 END, \
             window_started = CASE \
                 WHEN (CASE WHEN f.window_started + make_interval(secs => $5) <= $3 \
                            THEN 0 ELSE f.failures END) + 1 >= $4 THEN $3 \
                 WHEN f.window_started + make_interval(secs => $5) <= $3 THEN $3 \
                 ELSE f.window_started END, \
             locked_until = CASE \
                 WHEN (CASE WHEN f.window_started + make_interval(secs => $5) <= $3 \
                            THEN 0 ELSE f.failures END) + 1 >= $4 \
                     THEN $3 + make_interval(secs => $6) \
                 WHEN f.window_started + make_interval(secs => $5) <= $3 THEN NULL \
                 ELSE f.locked_until END, \
             expires_at = GREATEST( \
                 $3 + make_interval(secs => $5), \
                 CASE \
                     WHEN (CASE WHEN f.window_started + make_interval(secs => $5) <= $3 \
                                THEN 0 ELSE f.failures END) + 1 >= $4 \
                         THEN $3 + make_interval(secs => $6) \
                     WHEN f.window_started + make_interval(secs => $5) <= $3 THEN NULL \
                     ELSE f.locked_until END) \
         RETURNING (failures = 0 AND window_started = $3 \
                    AND locked_until IS NOT DISTINCT FROM $3 + make_interval(secs => $6)) \
                   AS tripped",
    )
    .bind::<Array<Text>, _>(kinds)
    .bind::<Array<Text>, _>(keys)
    .bind::<Timestamptz, _>(now)
    .bind::<Integer, _>(rules.max_failures)
    .bind::<Double, _>(rules.window_secs)
    .bind::<Double, _>(rules.lockout_secs)
    .load(conn)?;
    Ok(rows.iter().any(|r| r.tripped))
}

/// A success of `subjects` at `now`: remove the count of each subject that
/// is not locked out. A live lockout stays.
pub fn record_success(
    conn: &mut PgConnection,
    subjects: &[SubjectKey],
    now: DateTime<Utc>,
) -> QueryResult<usize> {
    if subjects.is_empty() {
        return Ok(0);
    }
    let (kinds, keys) = split(subjects);
    sql_query(
        "DELETE FROM public.auth_failures f \
         USING unnest($1::text[], $2::text[]) AS t(kind, subject) \
         WHERE f.kind = t.kind AND f.subject = t.subject \
           AND (f.locked_until IS NULL OR f.locked_until <= $3)",
    )
    .bind::<Array<Text>, _>(kinds)
    .bind::<Array<Text>, _>(keys)
    .bind::<Timestamptz, _>(now)
    .execute(conn)
}

/// Remove the rows that can change no decision at `now`: their window and
/// their lockout ended. The retention pass calls it (KAIROS-T-0377). The
/// number removed.
pub fn delete_expired(conn: &mut PgConnection, now: DateTime<Utc>) -> QueryResult<usize> {
    sql_query("DELETE FROM public.auth_failures WHERE expires_at <= $1")
        .bind::<Timestamptz, _>(now)
        .execute(conn)
}

/// The number of rows (for the tests).
pub fn count(conn: &mut PgConnection) -> QueryResult<i64> {
    #[derive(QueryableByName)]
    struct Row {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        n: i64,
    }
    sql_query("SELECT count(*) AS n FROM public.auth_failures")
        .get_result::<Row>(conn)
        .map(|r| r.n)
}
