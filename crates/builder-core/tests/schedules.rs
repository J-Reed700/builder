use builder_core::{
    schedule::{Access, Cadence, Definition, RunStatus, ScheduleState},
    store::Store,
};
use chrono::DateTime;
fn timestamp(value: &str) -> i64 {
    DateTime::parse_from_rfc3339(value).unwrap().timestamp()
}
fn definition(home: &std::path::Path) -> Definition {
    Definition {
        name: "Build monitor".into(),
        prompt: "Inspect recent failures".into(),
        workspace: home.canonicalize().unwrap(),
        profile: "local".into(),
        cadence: Cadence::Every { seconds: 60 },
        access: Access::ReadOnly,
        max_rounds: 10,
        timeout_seconds: 60,
    }
}
#[test]
fn interval_is_exact_and_coalesces_sleep_without_drift() {
    let cadence = Cadence::Every { seconds: 90 * 60 };
    assert_eq!(cadence.first(100).unwrap(), 5500);
    assert_eq!(
        cadence.next_after(5500, 5500 + 3 * 5400 + 20).unwrap(),
        Some(5500 + 4 * 5400)
    );
    assert!(Cadence::Every { seconds: 59 }.validate().is_err());
    for bad in ["", "💥", "0m", "-1h", "18446744073709551615d"] {
        assert!(builder_core::schedule::duration(bad).is_err());
    }
}
#[test]
fn calendar_respects_timezone_and_daylight_saving() {
    let cadence = Cadence::Cron {
        expression: "0 9 * * MON-FRI".into(),
        timezone: "America/Chicago".into(),
    };
    let friday = timestamp("2026-03-06T09:00:00-06:00");
    assert_eq!(
        cadence.first(friday).unwrap(),
        timestamp("2026-03-09T09:00:00-05:00")
    );
    let missing = Cadence::Cron {
        expression: "30 2 * * *".into(),
        timezone: "America/Chicago".into(),
    };
    assert_eq!(
        missing
            .first(timestamp("2026-03-08T00:00:00-06:00"))
            .unwrap(),
        timestamp("2026-03-08T03:00:00-05:00")
    );
    assert!(
        Cadence::Cron {
            expression: "0 9 * * *".into(),
            timezone: "not/a-zone".into()
        }
        .validate()
        .is_err()
    );
    assert!(
        Cadence::Cron {
            expression: "0 0 9 * * *".into(),
            timezone: "UTC".into()
        }
        .validate()
        .is_err()
    );
}
#[test]
fn persisted_occurrences_are_claimed_once_and_slow_runs_skip_backlog() {
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let schedule = store
        .schedule_create(&definition(home.path()), 1000)
        .unwrap();
    drop(store);
    let mut store = Store::open(home.path()).unwrap();
    let owner = store.scheduler_lock().unwrap();
    assert!(store.schedule_claim(&owner, 1059).unwrap().is_none());
    let (_, run) = store.schedule_claim(&owner, 2000).unwrap().unwrap();
    assert_eq!(run.occurrence, 1060);
    assert!(store.schedule_claim(&owner, 3000).unwrap().is_none());
    assert!(store.schedule_enqueue(&schedule.id, 3000).is_err());
    store
        .schedule_finish(&run.id, RunStatus::Succeeded, "ok", 3000)
        .unwrap();
    assert_eq!(store.schedule_get(&schedule.id).unwrap().next_due, 3040);
    assert!(store.schedule_claim(&owner, 3000).unwrap().is_none());
    assert_eq!(store.schedule_runs(&schedule.id, 20).unwrap().len(), 1);
}
#[test]
fn runner_ownership_is_exclusive_and_released_with_process_guard() {
    let home = tempfile::tempdir().unwrap();
    let a = Store::open(home.path()).unwrap();
    let b = Store::open(home.path()).unwrap();
    assert!(!a.scheduler_running().unwrap());
    let owner = a.scheduler_lock().unwrap();
    assert!(b.scheduler_running().unwrap());
    assert!(b.scheduler_lock().is_err());
    drop(owner);
    assert!(!b.scheduler_running().unwrap());
    assert!(b.scheduler_lock().is_ok());
}
#[test]
fn abandoned_runs_pause_and_never_replay_after_restart() {
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let schedule = store
        .schedule_create(&definition(home.path()), 1000)
        .unwrap();
    let owner = store.scheduler_lock().unwrap();
    let (_, run) = store.schedule_claim(&owner, 1060).unwrap().unwrap();
    let session = store
        .create("test", "local", home.path(), "system")
        .unwrap();
    store.schedule_attach_session(&run.id, &session).unwrap();
    drop(owner);
    drop(store);
    let mut store = Store::open(home.path()).unwrap();
    let owner = store.scheduler_lock().unwrap();
    assert_eq!(store.schedule_recover(&owner, 2000).unwrap(), 1);
    assert_eq!(store.schedule_recover(&owner, 2001).unwrap(), 0);
    assert!(store.schedule_claim(&owner, 3000).unwrap().is_none());
    assert!(matches!(
        store.schedule_get(&schedule.id).unwrap().state,
        ScheduleState::Paused
    ));
    let runs = store.schedule_runs(&schedule.id, 20).unwrap();
    assert_eq!(runs[0].status, RunStatus::Interrupted);
    assert_eq!(runs[0].session_id.as_deref(), Some(session.as_str()));
    store.schedule_resume(&schedule.id, 3000).unwrap();
    assert_eq!(store.schedule_get(&schedule.id).unwrap().next_due, 3060);
}
#[test]
fn pause_cancels_queue_delete_preserves_history_and_running_work_is_honest() {
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let schedule = store
        .schedule_create(&definition(home.path()), 1000)
        .unwrap();
    store.schedule_enqueue(&schedule.id, 1001).unwrap();
    store.schedule_pause(&schedule.id, false, 1002).unwrap();
    assert_eq!(
        store.schedule_runs(&schedule.id, 20).unwrap()[0].status,
        RunStatus::Cancelled
    );
    store.schedule_enqueue(&schedule.id, 1003).unwrap();
    let owner = store.scheduler_lock().unwrap();
    let (_, run) = store.schedule_claim(&owner, 1003).unwrap().unwrap();
    store.schedule_pause(&schedule.id, true, 1004).unwrap();
    assert!(store.schedule_resume(&schedule.id, 1005).is_err());
    assert!(store.schedule_enqueue(&schedule.id, 1005).is_err());
    assert_eq!(
        store.schedule_runs(&schedule.id, 20).unwrap()[0].status,
        RunStatus::Running
    );
    store
        .schedule_finish(&run.id, RunStatus::Failed, "test", 1006)
        .unwrap();
    assert!(matches!(
        store.schedule_get(&schedule.id).unwrap().state,
        ScheduleState::Deleted
    ));
    assert!(store.schedules().unwrap().is_empty());
    assert_eq!(store.schedule_runs(&schedule.id, 20).unwrap().len(), 2);
}
#[test]
fn overdue_one_shot_fires_once_and_failure_requires_explicit_action() {
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let mut def = definition(home.path());
    def.cadence = Cadence::At { timestamp: 1100 };
    let schedule = store.schedule_create(&def, 1000).unwrap();
    let owner = store.scheduler_lock().unwrap();
    let (_, run) = store.schedule_claim(&owner, 5000).unwrap().unwrap();
    store
        .schedule_finish(&run.id, RunStatus::Succeeded, "ok", 5001)
        .unwrap();
    assert!(matches!(
        store.schedule_get(&schedule.id).unwrap().state,
        ScheduleState::Completed
    ));
    assert!(store.schedule_claim(&owner, 6000).unwrap().is_none());
    store.schedule_enqueue(&schedule.id, 6000).unwrap();
    let (_, run) = store.schedule_claim(&owner, 6000).unwrap().unwrap();
    store
        .schedule_finish(&run.id, RunStatus::Blocked, "permission", 6001)
        .unwrap();
    assert!(matches!(
        store.schedule_get(&schedule.id).unwrap().state,
        ScheduleState::Paused
    ));
}
#[test]
fn schema_thirteen_upgrade_preserves_conversations() {
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let id = store
        .create("keep me", "local", home.path(), "original instructions")
        .unwrap();
    drop(store);
    let db = rusqlite::Connection::open(home.path().join("builder.sqlite3")).unwrap();
    db.execute_batch("DROP TABLE schedule_runs; DROP TABLE schedules; PRAGMA user_version=13;")
        .unwrap();
    drop(db);
    let mut store = Store::open(home.path()).unwrap();
    assert_eq!(store.session(&id).unwrap().title, "keep me");
    assert_eq!(
        store.messages(&id).unwrap()[0].content.as_deref(),
        Some("original instructions")
    );
    assert!(
        store
            .schedule_create(&definition(home.path()), 1000)
            .is_ok()
    );
}

#[test]
fn calendar_uses_conventional_weekday_numbers_and_day_or_semantics() {
    let weekdays = Cadence::Cron {
        expression: "0 9 * * 1-5".into(),
        timezone: "UTC".into(),
    };
    assert_eq!(
        weekdays.first(timestamp("2026-09-18T09:00:00Z")).unwrap(),
        timestamp("2026-09-21T09:00:00Z")
    );
    for sunday in ["0", "7", "SUN"] {
        let cadence = Cadence::Cron {
            expression: format!("0 9 * * {sunday}"),
            timezone: "UTC".into(),
        };
        assert_eq!(
            cadence.first(timestamp("2026-09-18T09:00:00Z")).unwrap(),
            timestamp("2026-09-20T09:00:00Z")
        );
    }
    // Conventional cron matches either the restricted month-day or weekday.
    let either = Cadence::Cron {
        expression: "0 9 1 * MON".into(),
        timezone: "UTC".into(),
    };
    assert_eq!(
        either.first(timestamp("2026-09-18T09:00:00Z")).unwrap(),
        timestamp("2026-09-21T09:00:00Z")
    );
}

#[test]
fn fixed_calendar_time_runs_once_during_the_fall_overlap() {
    let cadence = Cadence::Cron {
        expression: "30 1 * * *".into(),
        timezone: "America/Chicago".into(),
    };
    let first = cadence
        .first(timestamp("2026-11-01T00:00:00-05:00"))
        .unwrap();
    assert_eq!(first, timestamp("2026-11-01T01:30:00-05:00"));
    assert_eq!(
        cadence.next_after(first, first).unwrap(),
        Some(timestamp("2026-11-02T01:30:00-06:00"))
    );
}
